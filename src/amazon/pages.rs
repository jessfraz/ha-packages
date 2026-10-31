//! Strict, bounded projections of Amazon's retail order and tracking pages.
//! Incomplete identity is deferred, never turned into a synthetic shipment.

use std::collections::{BTreeMap, BTreeSet};

use reqwest::Url;
use scraper::{Html, Selector};
use serde::Deserialize;

use crate::{
    amazon::{DeferredCounts, provider_error},
    carrier::Carrier,
    models::{CarrierTracking, Status, Tracking},
    shipping_api::{
        error::{Error, ErrorKind},
        types::{ProviderId, Tracker},
    },
};

pub(crate) const ORIGIN: &str = "https://www.amazon.com";

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum PackageSlot {
    // Opaque package IDs and zero-based slot indices are different identities.
    Id(String),
    Index(String),
}

impl PackageSlot {
    pub(crate) fn source_component(&self) -> String {
        match self {
            Self::Id(value) => value.clone(),
            Self::Index(value) => format!("index:{value}"),
        }
    }

    fn package_id(&self) -> Option<&str> {
        match self {
            Self::Id(value) => Some(value),
            Self::Index(_) => None,
        }
    }
}

#[derive(Clone)]
pub(crate) struct ParcelPage {
    pub order: String,
    pub shipment: String,
    pub package: PackageSlot,
    pub path: String,
    pub titles: Vec<String>,
}

pub(crate) struct Listing {
    pub parcels: Vec<ParcelPage>,
    pub deferred: usize,
    pub deferred_reasons: DeferredCounts,
}

fn selector(value: &str) -> Result<Selector, Error> {
    Selector::parse(value).map_err(|_| provider_error(ErrorKind::Configuration))
}

pub(crate) fn signed_out(page: &str) -> bool {
    Html::parse_document(page)
        .select(
            &match Selector::parse("input[type=password], form[name=signIn]") {
                Ok(selector) => selector,
                Err(_) => return false,
            },
        )
        .next()
        .is_some()
}

// Fixed shape labels only. Never retain page contents, CSRF values or order data.
pub(crate) fn orders_shape(page: &str) -> &'static str {
    if page.contains("captchacharacters") || page.contains("/errors/validateCaptcha") {
        "challenge"
    } else if page.contains("ab-your-orders-anticsrf-token") && page.contains("yourOrdersStateData")
    {
        "business_shell"
    } else {
        "unrecognized_html"
    }
}

pub(crate) fn page_error_shape(page: &str) -> &'static str {
    if page.contains("Sorry! Something went wrong!")
        || page.contains("/images/G/01/error/en_US/500_503.png")
        || page.contains("/images/G/01/error/en_US/title._TTD_.png")
    {
        "amazon_service_error_page"
    } else {
        "unclassified_error_page"
    }
}

fn diagnostic_page_shape(page: &str) -> Option<&'static str> {
    match orders_shape(page) {
        shape @ ("challenge" | "business_shell") => Some(shape),
        _ if signed_out(page) => Some("sign_in"),
        _ if page_error_shape(page) == "amazon_service_error_page" => {
            Some("amazon_service_error_page")
        }
        _ => None,
    }
}

fn retail_path(path: &str) -> &str {
    path.strip_prefix("/-/en")
        .filter(|tail| tail.starts_with('/'))
        .unwrap_or(path)
}

pub(crate) fn page_kind(path: &str) -> crate::amazon::connection::PageKind {
    use crate::amazon::connection::PageKind;
    let path = retail_path(path).split('?').next().unwrap_or(path);
    if is_tracking(path) {
        PageKind::Tracking
    } else if ["/your-orders/orders", "/gp/css/order-history"].contains(&path) {
        PageKind::Orders
    } else {
        PageKind::Item
    }
}

pub(crate) fn allowed_path(input: &str) -> Result<String, Error> {
    if input.len() > 2048 || input.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return Err(provider_error(ErrorKind::InvalidResponse));
    }
    let url = Url::parse(ORIGIN)
        .and_then(|base| base.join(input))
        .map_err(|_| provider_error(ErrorKind::InvalidResponse))?;
    let path = url.path();
    let retail = retail_path(path);
    let supported = ["/your-orders/orders", "/gp/css/order-history"].contains(&retail)
        || [
            "/gp/your-account/ship-track",
            "/progress-tracker/package",
            "/gp/your-account/order-details",
            "/your-orders/order-details",
            "/your-orders/pop",
        ]
        .iter()
        .any(|prefix| {
            retail == *prefix
                || retail
                    .strip_prefix(prefix)
                    .is_some_and(|tail| tail.starts_with("/ref="))
        });
    if url.scheme() != "https"
        || url.host_str() != Some("www.amazon.com")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some_and(|port| port != 443)
        || url.fragment().is_some()
        || !supported
    {
        return Err(provider_error(ErrorKind::InvalidResponse));
    }
    let mut canonical = Url::parse(ORIGIN).map_err(|_| provider_error(ErrorKind::Configuration))?;
    canonical.set_path(path.split("/ref=").next().unwrap_or(path));
    let order_details = retail.split("/ref=").next() == Some("/your-orders/order-details");
    let mut seen = BTreeSet::new();
    for (name, value) in url.query_pairs() {
        let key = match name.to_ascii_lowercase().as_str() {
            "orderid" if order_details => "orderID",
            "orderid" => "orderId",
            "shipmentid" => "shipmentId",
            "packageid" => "packageId",
            "packageindex" => "packageIndex",
            "lineitemid" => "lineItemId",
            "itemid" => "itemId",
            "trackingid" => "trackingId",
            _ => continue,
        };
        if !identity_part(&value) || !seen.insert(key) {
            return Err(provider_error(ErrorKind::InvalidResponse));
        }
        canonical.query_pairs_mut().append_pair(key, &value);
    }
    Ok(format!(
        "{}{}",
        canonical.path(),
        canonical
            .query()
            .map(|q| format!("?{q}"))
            .unwrap_or_default()
    ))
}

fn identity_part(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
}

struct ParcelIdentity {
    order: String,
    shipment: String,
    package: Option<String>,
    index: Option<String>,
}

impl ParcelIdentity {
    // Item-pop and tracking URLs can name the same slot differently. Link
    // resolution compares shared fields; redirects also retain all known fields.
    fn compatible_with(&self, other: &Self) -> bool {
        self.order == other.order
            && self.shipment == other.shipment
            && !self
                .package
                .as_ref()
                .zip(other.package.as_ref())
                .is_some_and(|(a, b)| a != b)
            && !self
                .index
                .as_ref()
                .zip(other.index.as_ref())
                .is_some_and(|(a, b)| a != b)
    }
}

fn query_identity(path: &str) -> Result<ParcelIdentity, Error> {
    let url = Url::parse(ORIGIN)
        .and_then(|base| base.join(path))
        .map_err(|_| provider_error(ErrorKind::InvalidResponse))?;
    let mut values = BTreeMap::new();
    for (key, value) in url.query_pairs() {
        let key = key.to_ascii_lowercase();
        if ["orderid", "shipmentid", "packageid", "packageindex"].contains(&key.as_str())
            && (!identity_part(&value) || values.insert(key, value.into_owned()).is_some())
        {
            return Err(provider_error(ErrorKind::InvalidResponse));
        }
    }
    Ok(ParcelIdentity {
        order: values
            .remove("orderid")
            .ok_or_else(|| provider_error(ErrorKind::InvalidResponse))?,
        shipment: values
            .remove("shipmentid")
            .ok_or_else(|| provider_error(ErrorKind::InvalidResponse))?,
        package: values.remove("packageid"),
        index: values.remove("packageindex"),
    })
}

pub(crate) fn same_parcel_identity(original: &str, destination: &str) -> bool {
    match (query_identity(original), query_identity(destination)) {
        (Ok(original), Ok(destination)) => {
            original.compatible_with(&destination)
                && original
                    .package
                    .as_ref()
                    .is_none_or(|package| destination.package.as_ref() == Some(package))
                && original
                    .index
                    .as_ref()
                    .is_none_or(|index| destination.index.as_ref() == Some(index))
        }
        (Err(_), Err(_)) => {
            (["/your-orders/orders", "/gp/css/order-history"].contains(&retail_path(original))
                && ["/your-orders/orders", "/gp/css/order-history"]
                    .contains(&retail_path(destination)))
                || order_details_identity(original)
                    .zip(order_details_identity(destination))
                    .is_some_and(|(original, destination)| original == destination)
        }
        _ => false,
    }
}

fn order_details_identity(path: &str) -> Option<String> {
    let path = allowed_path(path).ok()?;
    let url = Url::parse(ORIGIN).ok()?.join(&path).ok()?;
    if ![
        "/your-orders/order-details",
        "/gp/your-account/order-details",
    ]
    .contains(&retail_path(url.path()))
    {
        return None;
    }
    // Only an order-only page may redirect without a known parcel identity.
    let mut pairs = url.query_pairs();
    let (key, order) = pairs.next()?;
    if !key.eq_ignore_ascii_case("orderid") || pairs.next().is_some() {
        return None;
    }
    Some(order.into_owned())
}

pub(crate) fn listing(page: &str) -> Result<Listing, Error> {
    let html = Html::parse_document(page);
    let mut groups: BTreeMap<(String, String, PackageSlot), ParcelPage> = BTreeMap::new();
    let mut reasons = DeferredCounts::default();
    let mut found = false;
    for (index, node) in html
        .select(&selector("a.item-card__link")?)
        .take(501)
        .enumerate()
    {
        found = true;
        if index >= 500 {
            return Err(provider_error(ErrorKind::ResponseTooLarge));
        }
        let Some(path) = node
            .value()
            .attr("href")
            .and_then(|href| allowed_path(href).ok())
        else {
            reasons.unsupported_link += 1;
            continue;
        };
        let Ok(identity) = query_identity(&path) else {
            reasons.missing_identity += 1;
            continue;
        };
        let Some(package) = identity
            .package
            .map(PackageSlot::Id)
            .or_else(|| identity.index.map(PackageSlot::Index))
        else {
            reasons.missing_identity += 1;
            continue;
        };
        let Some(title) = node
            .value()
            .attr("aria-label")
            .and_then(|label| label.split_once(", Order placed "))
            .map(|(title, _)| title.trim())
            .filter(|title| {
                !title.is_empty() && title.len() <= 500 && !title.chars().any(char::is_control)
            })
        else {
            reasons.missing_product += 1;
            continue;
        };
        let parcel = groups
            .entry((
                identity.order.clone(),
                identity.shipment.clone(),
                package.clone(),
            ))
            .or_insert_with(|| ParcelPage {
                order: identity.order,
                shipment: identity.shipment,
                package,
                path,
                titles: Vec::new(),
            });
        if !parcel.titles.iter().any(|value| value == title) {
            parcel.titles.push(title.to_owned());
        }
    }
    if !found
        && !page.contains("You have not placed any orders")
        && !page.contains("You have no orders")
    {
        return Err(provider_error(ErrorKind::InvalidResponse));
    }
    Ok(Listing {
        parcels: groups.into_values().collect(),
        deferred: reasons.total(),
        deferred_reasons: reasons,
    })
}

pub(crate) fn order_tracking_pages(page: &str, order: &str) -> Result<Vec<ParcelPage>, Error> {
    if !identity_part(order) {
        return Err(provider_error(ErrorKind::InvalidResponse));
    }
    let mut parcels = BTreeMap::new();
    for node in Html::parse_document(page).select(&selector("a[href]")?) {
        let Some(path) = node
            .value()
            .attr("href")
            .and_then(|href| allowed_path(href).ok())
            .filter(|path| is_tracking(path))
        else {
            continue;
        };
        let Ok(identity) = query_identity(&path) else {
            continue;
        };
        if identity.order != order {
            continue;
        }
        let Some(package) = identity
            .package
            .map(PackageSlot::Id)
            .or_else(|| identity.index.map(PackageSlot::Index))
        else {
            continue;
        };
        parcels.entry(path.clone()).or_insert_with(|| ParcelPage {
            order: identity.order,
            shipment: identity.shipment,
            package,
            path,
            titles: Vec::new(),
        });
        if parcels.len() > 20 {
            return Err(provider_error(ErrorKind::ResponseTooLarge));
        }
    }
    Ok(parcels.into_values().collect())
}

pub(crate) fn order_details_failure_shape(page: &str, expected_order: &str) -> &'static str {
    if let Some(shape) = diagnostic_page_shape(page) {
        return shape;
    }
    let html = Html::parse_document(page);
    if Selector::parse("a.item-card__link, .order-card.js-order-card")
        .ok()
        .is_some_and(|selector| html.select(&selector).next().is_some())
    {
        return "order_details_listing_page";
    }
    let Ok(selector) = Selector::parse("a[href]") else {
        return "order_details_identity_mismatch";
    };
    let mut tracking_link = false;
    let mut missing_identity = false;
    let mut different_order = false;
    for node in html.select(&selector) {
        let Some(path) = node
            .value()
            .attr("href")
            .and_then(|href| allowed_path(href).ok())
            .filter(|path| is_tracking(path))
        else {
            continue;
        };
        tracking_link = true;
        let Ok(identity) = query_identity(&path) else {
            missing_identity = true;
            continue;
        };
        if identity.package.is_none() && identity.index.is_none() {
            missing_identity = true;
        } else if identity.order != expected_order {
            different_order = true;
        } else {
            return "order_details_identity_mismatch";
        }
    }
    if !tracking_link {
        "order_details_tracking_links_missing"
    } else if missing_identity {
        "order_details_tracking_identity_missing"
    } else if different_order {
        "order_details_tracking_order_mismatch"
    } else {
        "order_details_identity_mismatch"
    }
}

pub(crate) fn tracking_link(page: &str, parcel: &ParcelPage) -> Result<String, Error> {
    let identity = query_identity(&parcel.path)?;
    let mut links = BTreeSet::new();
    for node in Html::parse_document(page).select(&selector("a[href]")?) {
        let Some(path) = node
            .value()
            .attr("href")
            .and_then(|href| allowed_path(href).ok())
        else {
            continue;
        };
        if is_tracking(&path)
            && query_identity(&path).is_ok_and(|other| identity.compatible_with(&other))
        {
            links.insert(path);
        }
    }
    if links.len() != 1 {
        return Err(provider_error(ErrorKind::InvalidResponse));
    }
    links
        .into_iter()
        .next()
        .ok_or_else(|| provider_error(ErrorKind::InvalidResponse))
}

pub(crate) fn is_tracking(path: &str) -> bool {
    let path = retail_path(path);
    path.starts_with("/gp/your-account/ship-track") || path.starts_with("/progress-tracker/package")
}

#[derive(Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct PageState {
    tracking_id: Option<String>,
    carrier_code: Option<String>,
    short_status: Option<String>,
    progress_tracker: Option<Progress>,
    order_id: Option<String>,
    shipment_id: Option<String>,
    package_id: Option<String>,
    package_index: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Progress {
    last_reached_milestone: Option<String>,
}

fn page_state_blocks(html: &Html) -> Result<Vec<String>, Error> {
    Ok(html
        .select(&selector("script")?)
        .filter(|script| {
            script
                .value()
                .attrs()
                .any(|(key, value)| key.contains("page-state") || value.contains("page-state"))
        })
        .map(|script| script.text().collect())
        .collect())
}

fn header_carriers(html: &Html) -> Result<Vec<Carrier>, Error> {
    let mut carriers = Vec::new();
    for header in html.select(&selector("h2, h3")?) {
        let text = header.text().collect::<Vec<_>>().join(" ");
        let text = text.trim();
        if let Some(carrier) = text
            .strip_prefix("Delivery By ")
            .or_else(|| text.strip_prefix("Delivery facilitated by "))
            .and_then(carrier)
            && !carriers.contains(&carrier)
        {
            carriers.push(carrier);
        }
    }
    Ok(carriers)
}

fn carrier(value: &str) -> Option<Carrier> {
    match value.trim().to_ascii_uppercase().as_str() {
        "AMAZON" | "AMZN_US" | "AMZL_US" | "AMAZON LOGISTICS" => Some(Carrier::AmazonAccount),
        "USPS" | "US_POSTAL_SERVICE" => Some(Carrier::Usps),
        "UPS" => Some(Carrier::Ups),
        "FEDEX" | "FED_EX" => Some(Carrier::FedExDefault),
        "ONTRAC" | "LASERSHIP" => Some(Carrier::OnTrac),
        "DHL" | "DHL_EXPRESS" => Some(Carrier::DhlExpress),
        _ => None,
    }
}

pub(crate) fn supports_carrier(value: Carrier) -> bool {
    matches!(
        value,
        Carrier::AmazonAccount
            | Carrier::AmazonShipping
            | Carrier::Usps
            | Carrier::Ups
            | Carrier::FedExDefault
            | Carrier::OnTrac
            | Carrier::DhlExpress
    )
}

fn status(value: &str) -> Option<Status> {
    match value {
        "ORDERED" | "ORDER_PLACED" => Some(Status::PreTransit),
        "SHIPPED" | "IN_TRANSIT" => Some(Status::InTransit),
        "OUT_FOR_DELIVERY" => Some(Status::OutForDelivery),
        "DELIVERED" | "PICKED_UP" => Some(Status::Delivered),
        _ => None,
    }
}

fn observed_status(state: &PageState) -> Option<Status> {
    state
        .progress_tracker
        .as_ref()
        .and_then(|progress| progress.last_reached_milestone.as_deref())
        .and_then(status)
        .or_else(|| state.short_status.as_deref().and_then(status))
}

pub(crate) fn tracking_failure_shape(page: &str) -> &'static str {
    if let Some(shape) = diagnostic_page_shape(page) {
        return shape;
    }
    let html = Html::parse_document(page);
    let Ok(mut states) = page_state_blocks(&html) else {
        return "tracking_page_state_invalid_shape";
    };
    if states.is_empty() {
        return "tracking_page_state_missing";
    }
    if states.len() != 1 {
        return "tracking_page_state_multiple";
    }
    let value = states.remove(0);
    let state = match serde_json::from_str::<PageState>(&value) {
        Ok(state) => state,
        Err(_) => {
            // Inspect only safe type kinds at this dynamic JSON boundary.
            let Ok(value) = serde_json::from_str::<serde_json::Value>(&value) else {
                return "tracking_page_state_invalid_json";
            };
            if value
                .get("packageId")
                .is_some_and(serde_json::Value::is_number)
            {
                return "tracking_package_id_numeric";
            }
            if value
                .get("packageIndex")
                .is_some_and(serde_json::Value::is_number)
            {
                return "tracking_package_index_numeric";
            }
            return "tracking_page_state_invalid_shape";
        }
    };
    let Ok(headers) = header_carriers(&html) else {
        return "tracking_page_state_invalid_shape";
    };
    if state.carrier_code.as_deref().and_then(carrier).is_none() && headers.is_empty() {
        return "tracking_carrier_missing";
    }
    let text = html.root_element().text().collect::<Vec<_>>().join(" ");
    let visible_number = text
        .split_once("Tracking ID:")
        .and_then(|(_, tail)| tail.split_whitespace().next());
    if state
        .tracking_id
        .as_deref()
        .is_none_or(|number| number.trim().is_empty())
        && visible_number.is_none()
    {
        return "tracking_number_missing";
    }
    if observed_status(&state).is_none() {
        return "tracking_status_unsupported";
    }
    "tracking_identity_mismatch"
}

pub(crate) fn tracking(page: &str, path: &str, expected: Option<&str>) -> Result<Tracker, Error> {
    tracking_with_package(page, path, expected, None)
}

pub(crate) fn tracking_parcel(
    page: &str,
    path: &str,
    parcel: &ParcelPage,
) -> Result<Tracker, Error> {
    if !query_identity(&parcel.path)?.compatible_with(&query_identity(path)?) {
        return Err(provider_error(ErrorKind::InvalidResponse));
    }
    tracking_with_package(page, path, None, parcel.package.package_id())
}

fn tracking_with_package(
    page: &str,
    path: &str,
    expected: Option<&str>,
    package: Option<&str>,
) -> Result<Tracker, Error> {
    let path = allowed_path(path)?;
    if !is_tracking(&path) {
        return Err(provider_error(ErrorKind::InvalidResponse));
    }
    let identity = query_identity(&path)?;
    let html = Html::parse_document(page);
    let mut states = Vec::new();
    for value in page_state_blocks(&html)? {
        let state: PageState =
            serde_json::from_str(&value).map_err(|_| provider_error(ErrorKind::InvalidResponse))?;
        states.push(state);
    }
    if states.len() != 1 {
        return Err(provider_error(ErrorKind::InvalidResponse));
    }
    let state = states.remove(0);
    let expected_package = identity.package.as_deref().or(package);
    if identity
        .package
        .as_deref()
        .zip(package)
        .is_some_and(|(a, b)| a != b)
        || [
            (state.order_id.as_deref(), Some(identity.order.as_str())),
            (
                state.shipment_id.as_deref(),
                Some(identity.shipment.as_str()),
            ),
            (state.package_id.as_deref(), expected_package),
            (state.package_index.as_deref(), identity.index.as_deref()),
        ]
        .into_iter()
        .any(|(observed, expected)| observed.zip(expected).is_some_and(|(a, b)| a != b))
    {
        return Err(provider_error(ErrorKind::InvalidResponse));
    }
    let text = html.root_element().text().collect::<Vec<_>>().join(" ");
    let state_carrier = state.carrier_code.as_deref().and_then(carrier);
    let header_carriers = header_carriers(&html)?;
    if header_carriers.len() > 1
        || state_carrier
            .zip(header_carriers.first().copied())
            .is_some_and(|(state, header)| state != header)
    {
        return Err(provider_error(ErrorKind::InvalidResponse));
    }
    let actual_carrier = state_carrier
        .or_else(|| header_carriers.first().copied())
        .ok_or_else(|| provider_error(ErrorKind::InvalidResponse))?;
    let canonical_number = |value: &str| -> Result<String, Error> {
        let mut tracking = Tracking {
            carrier: actual_carrier,
            number: value.into(),
            recipient_url: None,
        };
        crate::store::normalize_tracking(&mut tracking)
            .map_err(|_| provider_error(ErrorKind::InvalidResponse))?;
        crate::store::validate_admission(&tracking.clone().into())
            .map_err(|_| provider_error(ErrorKind::InvalidResponse))?;
        Ok(tracking.number)
    };
    let state_number = state
        .tracking_id
        .as_deref()
        .map(canonical_number)
        .transpose()?;
    let visible_number = text
        .split_once("Tracking ID:")
        .and_then(|(_, tail)| tail.split_whitespace().next())
        .map(canonical_number)
        .transpose()?;
    if state_number
        .as_ref()
        .zip(visible_number.as_ref())
        .is_some_and(|(state, visible)| state != visible)
    {
        return Err(provider_error(ErrorKind::InvalidResponse));
    }
    let number = state_number
        .or(visible_number)
        .ok_or_else(|| provider_error(ErrorKind::InvalidResponse))?;
    if expected.is_some_and(|expected| expected != number) {
        return Err(provider_error(ErrorKind::InvalidResponse));
    }
    let actual_status =
        observed_status(&state).ok_or_else(|| provider_error(ErrorKind::InvalidResponse))?;
    Ok(Tracker {
        provider: ProviderId::AmazonAccount,
        id: path.clone(),
        tracking_code: number,
        carrier: actual_carrier,
        status: actual_status,
        public_url: Some(format!("{ORIGIN}{path}")),
        carrier_tracking: CarrierTracking::default(),
    })
}

#[cfg(test)]
mod tests {
    use crate::{amazon::pages, carrier::Carrier, models::Status};
    const PATH: &str =
        "/gp/your-account/ship-track?orderId=111-1234567-1234567&shipmentId=SHIP1&packageId=1";
    pub(crate) fn page(number: &str) -> String {
        format!(
            r#"<h3>Delivery By USPS</h3><script id="page-state">{{"trackingId":"{number}","shortStatus":"IN_TRANSIT"}}</script>"#
        )
    }

    #[test]
    fn tracking_failures_report_only_fixed_producer_guard_shapes() {
        let number = "9400111122223333444455";
        for (html, expected) in [
            (
                "<main>PRIVATE_PAGE_BODY</main>".to_owned(),
                "tracking_page_state_missing",
            ),
            (page(number) + &page(number), "tracking_page_state_multiple"),
            (
                r#"<script id="page-state">{"trackingId":"PRIVATE_VALUE", broken}</script>"#
                    .to_owned(),
                "tracking_page_state_invalid_json",
            ),
            (
                r#"<script id="page-state">{"trackingId":["PRIVATE_VALUE"]}</script>"#
                    .to_owned(),
                "tracking_page_state_invalid_shape",
            ),
            (
                r#"<script id="page-state">{"packageId":1,"trackingId":"PRIVATE_VALUE"}</script>"#
                    .to_owned(),
                "tracking_package_id_numeric",
            ),
            (
                r#"<script id="page-state">{"packageIndex":0,"trackingId":"PRIVATE_VALUE"}</script>"#
                    .to_owned(),
                "tracking_package_index_numeric",
            ),
            (
                page(number).replace("Delivery By USPS", "PRIVATE_CARRIER"),
                "tracking_carrier_missing",
            ),
            (page(""), "tracking_number_missing"),
            (
                page(number).replace("IN_TRANSIT", "PRIVATE_UNKNOWN_STATUS"),
                "tracking_status_unsupported",
            ),
            (
                page(number).replace(
                    "\"trackingId\":",
                    "\"orderId\":\"111-1234567-7654321\",\"trackingId\":",
                ),
                "tracking_identity_mismatch",
            ),
        ] {
            assert!(pages::tracking(&html, PATH, None).is_err());
            assert_eq!(pages::tracking_failure_shape(&html), expected);
        }
    }

    #[test]
    fn order_details_failures_distinguish_missing_foreign_and_listing_markup() {
        const ORDER: &str = "111-1234567-1234567";
        for (html, expected) in [
            (
                "<main>PRIVATE_PAGE_BODY</main>",
                "order_details_tracking_links_missing",
            ),
            (
                r#"<a href="/progress-tracker/package?orderId=111-1234567-1234567&packageIndex=0">Track package</a>"#,
                "order_details_tracking_identity_missing",
            ),
            (
                r#"<a href="/progress-tracker/package?orderId=111-1234567-7654321&shipmentId=SHIP1&packageIndex=0">Track package</a>"#,
                "order_details_tracking_order_mismatch",
            ),
            (
                r#"<a href="/progress-tracker/package/delivery-information/delivery-feedback?orderId=111-1234567-1234567&shipmentId=SHIP1&packageIndex=0">Delivery feedback</a>"#,
                "order_details_tracking_links_missing",
            ),
            (
                r#"<a class="item-card__link" href="/your-orders/pop">PRIVATE_PRODUCT</a>"#,
                "order_details_listing_page",
            ),
            (
                r#"<div class="order-card js-order-card">PRIVATE_PRODUCT</div>"#,
                "order_details_listing_page",
            ),
        ] {
            assert!(pages::order_tracking_pages(html, ORDER).unwrap().is_empty());
            assert_eq!(pages::order_details_failure_shape(html, ORDER), expected);
        }
        let valid = r#"<a href="/progress-tracker/package?orderId=111-1234567-1234567&shipmentId=SHIP1&packageIndex=0">Track package</a>"#;
        assert_eq!(pages::order_tracking_pages(valid, ORDER).unwrap().len(), 1);
        assert_eq!(
            pages::order_details_failure_shape(valid, ORDER),
            "order_details_identity_mismatch"
        );
    }

    #[test]
    fn positive_known_page_shapes_are_not_guessed_from_error_words() {
        for (html, expected) in [
            ("<input id=captchacharacters>", "challenge"),
            ("<input type=password value=PRIVATE_VALUE>", "sign_in"),
            (
                "<script>ab-your-orders-anticsrf-token yourOrdersStateData PRIVATE_VALUE</script>",
                "business_shell",
            ),
            (
                "<title>Sorry! Something went wrong!</title>",
                "amazon_service_error_page",
            ),
            (
                r#"<img src="https://images-na.ssl-images-amazon.com/images/G/01/error/en_US/500_503.png">"#,
                "amazon_service_error_page",
            ),
        ] {
            assert_eq!(pages::tracking_failure_shape(html), expected);
            assert_eq!(pages::order_details_failure_shape(html, "ORDER"), expected);
        }
        assert_eq!(
            pages::page_error_shape("<main>503 CAPTCHA ban PRIVATE_VALUE</main>"),
            "unclassified_error_page"
        );
        assert_eq!(
            pages::page_error_shape("<input id=captchacharacters>"),
            "unclassified_error_page"
        );
        assert_eq!(
            pages::orders_shape("<input id=captchacharacters>"),
            "challenge"
        );
    }

    #[test]
    fn split_packages_keep_their_own_items() {
        let html = format!(
            r#"<a class="item-card__link" href="{PATH}" aria-label="Widget, Order placed 1 October 2026, Shipped"></a><a class="item-card__link" href="{PATH}" aria-label="Cable, Order placed 1 October 2026, Shipped"></a><a class="item-card__link" href="{}" aria-label="Book, Order placed 1 October 2026, Shipped"></a>"#,
            PATH.replace("packageId=1", "packageId=2")
        );
        let listing = pages::listing(&html).unwrap();
        assert_eq!(listing.parcels.len(), 2);
        assert_eq!(listing.parcels[0].titles, ["Widget", "Cable"]);
        assert_eq!(listing.parcels[1].titles, ["Book"]);
    }
    #[test]
    fn absent_real_package_identity_is_deferred() {
        let listing = pages::listing(r#"<a class="item-card__link" href="/gp/your-account/ship-track?orderId=111-1234567-1234567&lineItemId=LINE" aria-label="Widget, Order placed 1 October 2026, Shipped"></a>"#).unwrap();
        assert!(listing.parcels.is_empty());
        assert_eq!(listing.deferred, 1);
    }

    #[test]
    fn mobile_listing_keeps_real_indexed_packages_distinct() {
        let first =
            "/progress-tracker/package?orderId=111-1234567-1234567&shipmentId=SHIP1&packageIndex=0";
        let second = first.replace("packageIndex=0", "packageIndex=1");
        let listing = pages::listing(&format!(
            r#"<a class="item-card__link" href="{first}" aria-label="Widget, Order placed 1 October 2026, Shipped"></a><a class="item-card__link" href="{first}" aria-label="Cable, Order placed 1 October 2026, Shipped"></a><a class="item-card__link" href="{second}" aria-label="Book, Order placed 1 October 2026, Shipped"></a>"#
        ))
        .unwrap();
        assert_eq!(listing.deferred, 0);
        assert_eq!(listing.parcels.len(), 2);
        assert_eq!(listing.parcels[0].path, first);
        assert_eq!(listing.parcels[0].titles, ["Widget", "Cable"]);
        assert_eq!(listing.parcels[1].path, second);
        assert_eq!(listing.parcels[1].titles, ["Book"]);
    }

    #[test]
    fn package_ids_and_indexes_are_separate_listing_identities() {
        let path = "/progress-tracker/package?orderId=111-1234567-1234567&shipmentId=SHIP1";
        let listing = pages::listing(&format!(
            r#"<a class="item-card__link" href="{path}&packageId=0" aria-label="Opaque-ID parcel, Order placed 1 October 2026, Shipped"></a><a class="item-card__link" href="{path}&packageIndex=0" aria-label="Indexed parcel, Order placed 1 October 2026, Shipped"></a><a class="item-card__link" href="{path}&packageId=0&packageIndex=9" aria-label="Same ID extra item, Order placed 1 October 2026, Shipped"></a>"#
        ))
        .unwrap();
        assert_eq!(listing.deferred, 0);
        assert_eq!(listing.parcels.len(), 2);
        assert_eq!(
            listing.parcels[0].titles,
            ["Opaque-ID parcel", "Same ID extra item"]
        );
        assert_eq!(listing.parcels[1].titles, ["Indexed parcel"]);
    }

    #[test]
    fn indexed_package_tracking_rejects_a_different_page_state_slot() {
        let path =
            "/progress-tracker/package?orderId=111-1234567-1234567&shipmentId=SHIP1&packageIndex=0";
        let matching = page("9400111122223333444455")
            .replace("\"trackingId\":", "\"packageIndex\":\"0\",\"trackingId\":");
        assert!(pages::tracking(&matching, path, None).is_ok());
        let different = matching.replace("\"packageIndex\":\"0\"", "\"packageIndex\":\"1\"");
        assert!(pages::tracking(&different, path, None).is_err());
    }

    #[test]
    fn facilitated_amazon_delivery_header_verifies_the_observed_tracking_structure() {
        let path =
            "/progress-tracker/package?orderId=111-1234567-1234567&shipmentId=SHIP1&packageIndex=0";
        let html = r#"<h3 class="a-spacing-small">Delivery facilitated by Amazon</h3><div class="a-row tracking-event-carrier-header"><h2>Delivery facilitated by Amazon</h2></div><script id="page-state">{"trackingId":"TBA000000000020","orderId":"111-1234567-1234567","shipmentId":"SHIP1","packageIndex":"0","shortStatus":"DELIVERED","progressTracker":{"lastReachedMilestone":"DELIVERED"}}</script>"#;
        let tracker = pages::tracking(html, path, Some("TBA000000000020")).unwrap();
        assert_eq!(tracker.carrier.as_str(), "AmazonAccount");
        assert_eq!(
            crate::shipping_api::types::ProviderId::for_carrier(Some(tracker.carrier)).as_str(),
            "amazon_account"
        );
        assert_eq!(tracker.status, Status::Delivered);
        assert_eq!(tracker.tracking_code, "TBA000000000020");
    }

    #[test]
    fn facilitated_delivery_headers_must_agree_with_other_carrier_evidence() {
        for html in [
            page("9400111122223333444455")
                .replace("Delivery By USPS", "Delivery facilitated by Amazon")
                .replace(
                    "\"trackingId\":",
                    "\"carrierCode\":\"USPS\",\"trackingId\":",
                ),
            page("9400111122223333444455") + "<h3>Delivery facilitated by Amazon</h3>",
        ] {
            assert!(pages::tracking(&html, PATH, None).is_err());
        }
    }

    #[test]
    fn current_order_details_route_keeps_its_order_id_query_spelling() {
        let path = "/your-orders/order-details?orderID=111-1234567-1234567&ref=fixture";
        assert_eq!(
            pages::allowed_path(path).unwrap(),
            "/your-orders/order-details?orderID=111-1234567-1234567"
        );
    }

    #[test]
    fn order_only_details_redirects_retain_the_order_across_supported_routes() {
        let routes = [
            "/your-orders/order-details?orderID=111-1234567-1234567",
            "/gp/your-account/order-details?orderId=111-1234567-1234567",
            "/-/en/your-orders/order-details/ref=fixture?orderID=111-1234567-1234567",
            "/-/en/gp/your-account/order-details/ref=fixture?orderID=111-1234567-1234567",
        ];
        for original in routes {
            for destination in routes {
                assert!(pages::same_parcel_identity(original, destination));
            }
        }
    }

    #[test]
    fn order_details_redirects_never_drop_or_replace_known_identity() {
        const ORDER: &str = "/your-orders/order-details?orderID=111-1234567-1234567";
        for destination in [
            "/gp/your-account/order-details?orderId=111-1234567-7654321",
            "/gp/your-account/order-details",
            "/gp/your-account/order-details?orderId=111-1234567-1234567&orderId=111-1234567-7654321",
            "/your-orders/orders?orderId=111-1234567-1234567",
            "https://evil.example/your-orders/order-details?orderID=111-1234567-1234567",
        ] {
            assert!(!pages::same_parcel_identity(ORDER, destination));
        }
        for original in [
            format!("{ORDER}&shipmentId=SHIP1"),
            format!("{ORDER}&packageId=1"),
            format!("{ORDER}&packageIndex=0"),
            format!("{ORDER}&lineItemId=ITEM1"),
            format!("{ORDER}&itemId=ITEM1"),
            format!("{ORDER}&trackingId=TBA000000000020"),
        ] {
            assert!(!pages::same_parcel_identity(&original, ORDER));
        }
    }

    #[test]
    fn order_details_tracking_candidates_require_exact_order_and_real_package_identity() {
        let order = "111-1234567-1234567";
        let indexed =
            format!("/progress-tracker/package?orderId={order}&shipmentId=SHIP1&packageIndex=0");
        let identified =
            format!("/gp/your-account/ship-track?orderId={order}&shipmentId=SHIP2&packageId=1");
        let html = format!(
            r#"<a href="{indexed}">Track package</a><a href="{indexed}">Track package</a><a href="{identified}">Track package</a><a href="/progress-tracker/package?orderId=111-1234567-7654321&shipmentId=SHIP1&packageIndex=0">Other order</a><a href="/progress-tracker/package?orderId={order}&packageIndex=0">Missing shipment</a><a href="/progress-tracker/package?orderId={order}&shipmentId=SHIP1">Missing package slot</a><a href="/progress-tracker/package/delivery-information/delivery-feedback?orderId={order}&shipmentId=SHIP1&packageIndex=0">Delivery feedback</a>"#
        );
        let parcels = pages::order_tracking_pages(&html, order).unwrap();
        assert_eq!(parcels.len(), 2);
        assert!(parcels.iter().all(|parcel| parcel.order == order));
        assert!(parcels.iter().all(|parcel| parcel.titles.is_empty()));
        assert!(parcels.iter().any(|parcel| parcel.path == indexed));
        assert!(parcels.iter().any(|parcel| parcel.path == identified));
        assert!(
            pages::order_tracking_pages(&html, "111-0000000-0000000")
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn order_details_tracking_candidates_are_bounded_after_deduplication() {
        let order = "111-1234567-1234567";
        let link = |index: usize| {
            format!(
                r#"<a href="/progress-tracker/package?orderId={order}&shipmentId=SHIP1&packageIndex={index}">Track package</a>"#
            )
        };
        let maximum = (0..20).map(link).collect::<String>();
        assert_eq!(
            pages::order_tracking_pages(&maximum, order).unwrap().len(),
            20
        );
        assert!(pages::order_tracking_pages(&(maximum + &link(20)), order).is_err());
        assert_eq!(
            pages::order_tracking_pages(&link(0).repeat(21), order)
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn localized_pop_preserves_item_identity_and_checks_tracking_package_state() {
        let pop = "/-/en/your-orders/pop/ref=fixture?orderId=111-1234567-1234567&shipmentId=SHIP1&packageId=1&lineItemId=ITEM1";
        let listing = pages::listing(&format!(r#"<a class="item-card__link" href="{pop}" aria-label="Widget, Order placed 1 October 2026, Shipped"></a>"#)).unwrap();
        assert_eq!(listing.parcels.len(), 1);
        let parcel = &listing.parcels[0];
        assert!(parcel.path.contains("lineItemId=ITEM1"));
        let path = "/-/en/gp/your-account/ship-track?orderId=111-1234567-1234567&shipmentId=SHIP1&packageIndex=0&itemId=ITEM1";
        let found =
            pages::tracking_link(&format!(r#"<a href="{path}">Track</a>"#), parcel).unwrap();
        assert_eq!(found, path);
        let valid = page("9400111122223333444455");
        assert!(pages::tracking_parcel(&valid, &found, parcel).is_ok());
        let mismatch = valid.replace("\"trackingId\":", "\"packageId\":\"2\",\"trackingId\":");
        assert!(pages::tracking_parcel(&mismatch, &found, parcel).is_err());
        assert!(!pages::same_parcel_identity(
            path,
            &path.replace("packageIndex=0", "packageIndex=1")
        ));
        assert!(!pages::same_parcel_identity(
            path,
            &path.replace("&packageIndex=0", "")
        ));
        assert!(!pages::same_parcel_identity(
            PATH,
            &PATH.replace("&packageId=1", "")
        ));
    }

    #[test]
    fn item_pop_never_selects_an_ambiguous_or_different_shipment() {
        let listing=pages::listing(&format!(r#"<a class="item-card__link" href="{PATH}" aria-label="Widget, Order placed 1 October 2026, Shipped"></a>"#)).unwrap();
        let parcel = &listing.parcels[0];
        let first = PATH.replace("packageId=1", "packageIndex=0");
        let second = PATH.replace("packageId=1", "packageIndex=1");
        let ambiguous =
            format!(r#"<a href="{first}">Track</a><a href="{second}">Track another</a>"#);
        assert!(pages::tracking_link(&ambiguous, parcel).is_err());
        let other = first.replace("shipmentId=SHIP1", "shipmentId=SHIP2");
        assert!(pages::tracking_link(&format!(r#"<a href="{other}">Track</a>"#), parcel).is_err());
        let duplicates = format!(r#"<a href="{first}">Track</a><a href="{first}">Track again</a>"#);
        assert_eq!(pages::tracking_link(&duplicates, parcel).unwrap(), first);
    }

    #[test]
    fn incomplete_listing_reports_only_fixed_deferral_counts() {
        let missing = PATH.replace("&packageId=1", "");
        let html = format!(
            r#"<a class="item-card__link" href="/gp/buy" aria-label="PRIVATE_TITLE"></a><a class="item-card__link" href="{missing}" aria-label="PRIVATE_TITLE"></a><a class="item-card__link" href="{PATH}" aria-label="PRIVATE_TITLE"></a>"#
        );
        let listing = pages::listing(&html).unwrap();
        assert!(listing.parcels.is_empty());
        assert_eq!(listing.deferred, 3);
        assert_eq!(listing.deferred_reasons.unsupported_link, 1);
        assert_eq!(listing.deferred_reasons.missing_identity, 1);
        assert_eq!(listing.deferred_reasons.missing_product, 1);
        assert!(
            !serde_json::to_string(&listing.deferred_reasons)
                .unwrap()
                .contains("PRIVATE_TITLE")
        );
    }

    #[test]
    fn tracking_requires_real_number_carrier_and_known_state() {
        let tracker = pages::tracking(&page("9400111122223333444455"), PATH, None).unwrap();
        assert_eq!(tracker.carrier, Carrier::Usps);
        assert_eq!(tracker.status, Status::InTransit);
        assert!(pages::tracking(&page(""), PATH, None).is_err());
        assert!(pages::tracking(&page("A1"), PATH, None).is_err());
        assert_eq!(
            pages::tracking(&page("9400 111122223333444455"), PATH, None)
                .unwrap()
                .tracking_code,
            "9400111122223333444455"
        );
        assert!(pages::tracking(&page("9400111122223333444455"), PATH, Some("DIFFERENT")).is_err());
        assert!(
            pages::tracking(
                &page("9400111122223333444455").replace("USPS", "UnknownCarrier"),
                PATH,
                None
            )
            .is_err()
        );
        assert!(
            pages::tracking(
                &page("9400111122223333444455").replace("IN_TRANSIT", "TELEPORTED"),
                PATH,
                None
            )
            .is_err()
        );
    }
    #[test]
    fn unrecognised_empty_listing_is_an_error() {
        assert!(pages::listing("<html>CAPTCHA</html>").is_err());
        assert!(pages::listing("You have not placed any orders").is_ok());
    }
    #[test]
    fn urls_stay_within_allowed_amazon_routes() {
        for bad in [
            "https://evil.example/gp/your-account/ship-track",
            "https://www.amazon.com.evil.example/gp/your-account/ship-track",
            "https://user@www.amazon.com/gp/your-account/ship-track",
            "//evil.example/gp/your-account/ship-track",
            "/gp/your-account/ship-track/../../gp/buy",
            "/gp/buy",
            "/gp/your-account/ship-track#x",
        ] {
            assert!(pages::allowed_path(bad).is_err(), "{bad}");
        }
    }
    #[test]
    fn differing_visible_and_structured_identifiers_are_not_invented_aliases() {
        let conflict = page("9400111122223333444455") + "<span>Tracking ID: TBA123456789</span>";
        assert!(pages::tracking(&conflict, PATH, None).is_err());
        let same =
            page("9400111122223333444455") + "<span>Tracking ID: 9400111122223333444455</span>";
        assert_eq!(
            pages::tracking(&same, PATH, None).unwrap().tracking_code,
            "9400111122223333444455"
        );
    }
    #[test]
    fn conflicting_carrier_evidence_is_deferred() {
        let conflict = page("9400111122223333444455").replace(
            "\"trackingId\":",
            "\"carrierCode\":\"AMZN_US\",\"trackingId\":",
        );
        assert!(pages::tracking(&conflict, PATH, None).is_err());
        let same = conflict.replace("AMZN_US", "USPS");
        assert_eq!(
            pages::tracking(&same, PATH, None).unwrap().carrier,
            Carrier::Usps
        );
    }
}
