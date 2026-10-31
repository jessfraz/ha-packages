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

#[derive(Clone)]
pub(crate) struct ParcelPage {
    pub order: String,
    pub shipment: String,
    pub package: String,
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
    let mut seen = BTreeSet::new();
    for (name, value) in url.query_pairs() {
        let key = match name.to_ascii_lowercase().as_str() {
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
            ["/your-orders/orders", "/gp/css/order-history"].contains(&retail_path(original))
                && ["/your-orders/orders", "/gp/css/order-history"]
                    .contains(&retail_path(destination))
        }
        _ => false,
    }
}

pub(crate) fn listing(page: &str) -> Result<Listing, Error> {
    let html = Html::parse_document(page);
    let mut groups: BTreeMap<(String, String, String), ParcelPage> = BTreeMap::new();
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
        let Some(identity) = query_identity(&path)
            .ok()
            .filter(|value| value.package.is_some())
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
        let Some(package) = identity.package else {
            return Err(provider_error(ErrorKind::InvalidResponse));
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
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Progress {
    last_reached_milestone: Option<String>,
}

fn carrier(value: &str) -> Option<Carrier> {
    match value.trim().to_ascii_uppercase().as_str() {
        "AMAZON" | "AMZN_US" | "AMZL_US" | "AMAZON LOGISTICS" => Some(Carrier::AmazonMcf),
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
        Carrier::AmazonMcf
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
    tracking_with_package(page, path, None, Some(&parcel.package))
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
    for script in html.select(&selector("script")?) {
        if script
            .value()
            .attrs()
            .any(|(key, value)| key.contains("page-state") || value.contains("page-state"))
        {
            let value = script.text().collect::<String>();
            let state: PageState = serde_json::from_str(&value)
                .map_err(|_| provider_error(ErrorKind::InvalidResponse))?;
            states.push(state);
        }
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
        ]
        .into_iter()
        .any(|(observed, expected)| observed.zip(expected).is_some_and(|(a, b)| a != b))
    {
        return Err(provider_error(ErrorKind::InvalidResponse));
    }
    let text = html.root_element().text().collect::<Vec<_>>().join(" ");
    let state_carrier = state.carrier_code.as_deref().and_then(carrier);
    let mut header_carriers = Vec::new();
    for header in html.select(&selector("h2, h3")?) {
        let text = header.text().collect::<Vec<_>>().join(" ");
        if let Some(carrier) = text.trim().strip_prefix("Delivery By ").and_then(carrier)
            && !header_carriers.contains(&carrier)
        {
            header_carriers.push(carrier);
        }
    }
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
    let actual_status = state
        .progress_tracker
        .as_ref()
        .and_then(|p| p.last_reached_milestone.as_deref())
        .and_then(status)
        .or_else(|| state.short_status.as_deref().and_then(status))
        .ok_or_else(|| provider_error(ErrorKind::InvalidResponse))?;
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
