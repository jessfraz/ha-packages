//! Strict, bounded projections of Amazon's retail order and tracking pages.
//! Incomplete identity is deferred, never turned into a synthetic shipment.

use std::collections::{BTreeMap, BTreeSet, btree_map::Entry};

use reqwest::Url;
use scraper::{Html, Selector};
use serde::Deserialize;

use crate::{
    amazon::{DeferredCounts, provider_error},
    carrier::Carrier,
    models::{
        CarrierTracking, DeliveryDateValue, ProviderDeliveryDate, Status, Tracking,
        provider_optional,
    },
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
    pub next_page: Option<String>,
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

fn checkpoint_route(url: &Url, path: &str) -> bool {
    url.scheme() == "https"
        && url.host_str() == Some("www.amazon.com")
        && url.username().is_empty()
        && url.password().is_none()
        && url.port().is_none()
        && url.fragment().is_none()
        && url.path() == path
}

pub(crate) fn checkpoint_redirect(url: &Url) -> bool {
    checkpoint_route(url, "/checkpoint/continue")
}

fn checkpoint_form(page: &str) -> bool {
    if !page.contains("/checkpoint/validate") {
        return false;
    }
    let (Ok(forms), Ok(checkbox)) = (
        Selector::parse("form[action]"),
        Selector::parse("input[type=checkbox]"),
    ) else {
        return false;
    };
    Html::parse_document(page).select(&forms).any(|form| {
        let Some(action) = form.value().attr("action") else {
            return false;
        };
        // The attested form uses a root-relative action. Do not resolve an
        // arbitrary document-relative action against a guessed page location.
        (action.starts_with('/') || action.starts_with("https://"))
            && Url::parse(ORIGIN)
                .and_then(|base| base.join(action))
                .is_ok_and(|url| checkpoint_route(&url, "/checkpoint/validate"))
            && form.select(&checkbox).next().is_some()
    })
}

// Fixed shape labels only. Never retain page contents, CSRF values or order data.
pub(crate) fn orders_shape(page: &str) -> &'static str {
    if page.contains("captchacharacters")
        || page.contains("/errors/validateCaptcha")
        || checkpoint_form(page)
    {
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
    let listing = ["/your-orders/orders", "/gp/css/order-history"].contains(&retail);
    let order_details = retail.split("/ref=").next() == Some("/your-orders/order-details");
    let mut seen = BTreeSet::new();
    for (name, value) in url.query_pairs() {
        if listing {
            let (key, value) = match name.to_ascii_lowercase().as_str() {
                "page" => {
                    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
                        return Err(provider_error(ErrorKind::InvalidResponse));
                    }
                    let page = value
                        .parse::<u8>()
                        .ok()
                        .filter(|page| *page <= 100)
                        .ok_or_else(|| provider_error(ErrorKind::InvalidResponse))?;
                    ("page", page.to_string())
                }
                "startindex" => {
                    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
                        return Err(provider_error(ErrorKind::InvalidResponse));
                    }
                    let start = value
                        .parse::<u16>()
                        .ok()
                        .filter(|start| *start <= 1000 && *start % 10 == 0)
                        .ok_or_else(|| provider_error(ErrorKind::InvalidResponse))?;
                    ("startIndex", start.to_string())
                }
                "timefilter" if value == "months-3" => ("timeFilter", value.into_owned()),
                "timefilter" => return Err(provider_error(ErrorKind::InvalidResponse)),
                _ => continue,
            };
            if !seen.insert(key) || (seen.contains("page") && seen.contains("startIndex")) {
                return Err(provider_error(ErrorKind::InvalidResponse));
            }
            canonical.query_pairs_mut().append_pair(key, &value);
            continue;
        }
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

#[derive(Clone)]
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

    fn conflicts_with(&self, other: &Self) -> bool {
        let shared_id = self
            .package
            .as_ref()
            .zip(other.package.as_ref())
            .is_some_and(|(a, b)| a == b);
        let shared_index = self
            .index
            .as_ref()
            .zip(other.index.as_ref())
            .is_some_and(|(a, b)| a == b);
        self.order == other.order
            && self.shipment == other.shipment
            && (shared_id || shared_index)
            && !self.compatible_with(other)
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
            listing_identity(original)
                .zip(listing_identity(destination))
                .is_some_and(|(original, destination)| {
                    original.page == destination.page
                        && original
                            .time_filter
                            .as_ref()
                            .is_none_or(|filter| destination.time_filter.as_ref() == Some(filter))
                })
                || order_details_identity(original)
                    .zip(order_details_identity(destination))
                    .is_some_and(|(original, destination)| original == destination)
        }
        _ => false,
    }
}

#[derive(PartialEq, Eq)]
struct ListingIdentity {
    page: u8,
    time_filter: Option<String>,
}

fn listing_identity(path: &str) -> Option<ListingIdentity> {
    let path = allowed_path(path).ok()?;
    let url = Url::parse(ORIGIN).ok()?.join(&path).ok()?;
    if !["/your-orders/orders", "/gp/css/order-history"].contains(&retail_path(url.path())) {
        return None;
    }
    let mut page = 0;
    let mut filter = None;
    for (key, value) in url.query_pairs() {
        match key.as_ref() {
            "page" => page = value.parse().ok()?,
            "startIndex" => page = u8::try_from(value.parse::<u16>().ok()? / 10).ok()?,
            "timeFilter" => filter = Some(value.into_owned()),
            _ => return None,
        }
    }
    Some(ListingIdentity {
        page,
        time_filter: filter,
    })
}

/// Ordinal only, validated by the same strict route/parameter contract as redirects.
pub(crate) fn listing_page_index(path: &str) -> Option<u8> {
    listing_identity(path).map(|identity| identity.page)
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
    let mut groups: BTreeMap<(String, String, PackageSlot), usize> = BTreeMap::new();
    let mut parcels = Vec::new();
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
            // The response and anchor limits already bound source names.
            // Generated shipment labels have a separate display byte limit.
            .filter(|title| !title.is_empty() && !title.chars().any(char::is_control))
        else {
            reasons.missing_product += 1;
            continue;
        };
        let index = groups
            .entry((
                identity.order.clone(),
                identity.shipment.clone(),
                package.clone(),
            ))
            .or_insert_with(|| {
                let index = parcels.len();
                parcels.push(ParcelPage {
                    order: identity.order,
                    shipment: identity.shipment,
                    package,
                    path,
                    titles: Vec::new(),
                });
                index
            });
        let parcel = parcels
            .get_mut(*index)
            .ok_or_else(|| provider_error(ErrorKind::Configuration))?;
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
        parcels,
        next_page: next_listing_page(&html)?,
        deferred: reasons.total(),
        deferred_reasons: reasons,
    })
}

fn next_listing_page(html: &Html) -> Result<Option<String>, Error> {
    let mut next: Option<(String, ListingIdentity)> = None;
    #[derive(Deserialize)]
    struct StateKey {
        key: String,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct ChunkState {
        next_start_index: Option<u16>,
        time_filter: String,
    }
    for script in html.select(&selector(r#"script[type="a-state"][data-a-state]"#)?) {
        let Some(metadata) = script
            .value()
            .attr("data-a-state")
            .and_then(|value| serde_json::from_str::<StateKey>(value).ok())
        else {
            continue;
        };
        if metadata.key != "chunkState" {
            continue;
        }
        let state: ChunkState = serde_json::from_str(&script.text().collect::<String>())
            .map_err(|_| provider_error(ErrorKind::InvalidResponse))?;
        if state.time_filter != "months-3" {
            return Err(provider_error(ErrorKind::InvalidResponse));
        }
        // No chunkState (or an explicit null next index) is a normal final page.
        // Continuation tokens are unnecessary and never retained.
        let Some(index) = state.next_start_index else {
            continue;
        };
        if index == 0 || index > 1000 || index % 10 != 0 {
            return Err(provider_error(ErrorKind::InvalidResponse));
        }
        let path = format!("/your-orders/orders?startIndex={index}&timeFilter=months-3");
        let identity =
            listing_identity(&path).ok_or_else(|| provider_error(ErrorKind::InvalidResponse))?;
        if next
            .as_ref()
            .is_some_and(|(_, existing)| *existing != identity)
        {
            return Err(provider_error(ErrorKind::InvalidResponse));
        }
        next.get_or_insert((path, identity));
    }
    for anchor in html.select(&selector(
        ".a-pagination li.a-last a[href], a[rel~=next][href]",
    )?) {
        let relation = anchor
            .value()
            .attr("rel")
            .is_some_and(|value| value.split_whitespace().any(|value| value == "next"));
        let label = anchor.text().collect::<Vec<_>>().join(" ");
        if !relation
            && !label.trim().strip_prefix("Next").is_some_and(|tail| {
                tail.chars()
                    .all(|character| character.is_whitespace() || character == '→')
            })
        {
            return Err(provider_error(ErrorKind::InvalidResponse));
        }
        let path = allowed_path(
            anchor
                .value()
                .attr("href")
                .ok_or_else(|| provider_error(ErrorKind::InvalidResponse))?,
        )?;
        let identity =
            listing_identity(&path).ok_or_else(|| provider_error(ErrorKind::InvalidResponse))?;
        let url = Url::parse(ORIGIN)
            .and_then(|base| base.join(&path))
            .map_err(|_| provider_error(ErrorKind::InvalidResponse))?;
        if !url.query_pairs().any(|(key, _)| key == "page") {
            return Err(provider_error(ErrorKind::InvalidResponse));
        }
        if next
            .as_ref()
            .is_some_and(|(_, existing)| *existing != identity)
        {
            return Err(provider_error(ErrorKind::InvalidResponse));
        }
        next.get_or_insert((path, identity));
    }
    Ok(next.map(|(path, _)| path))
}

pub(crate) fn order_tracking_pages(page: &str, order: &str) -> Result<Vec<ParcelPage>, Error> {
    if !identity_part(order) {
        return Err(provider_error(ErrorKind::InvalidResponse));
    }
    let html = Html::parse_document(page);
    let mut parcels = BTreeMap::new();
    let mut identities = Vec::<ParcelIdentity>::new();
    for node in html.select(&selector("a[href]")?) {
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
            .clone()
            .map(PackageSlot::Id)
            .or_else(|| identity.index.clone().map(PackageSlot::Index))
        else {
            continue;
        };
        if let Entry::Vacant(entry) = parcels.entry(path.clone()) {
            // A shared explicit ID or index identifies one slot within its
            // shipment. Contradictory evidence cannot be resolved by URL order.
            if identities
                .iter()
                .any(|previous| identity.conflicts_with(previous))
            {
                return Err(provider_error(ErrorKind::InvalidResponse));
            }
            identities.push(identity.clone());
            entry.insert(ParcelPage {
                order: identity.order,
                shipment: identity.shipment,
                package,
                path,
                titles: Vec::new(),
            });
        }
        if parcels.len() > 20 {
            return Err(provider_error(ErrorKind::ResponseTooLarge));
        }
    }
    bind_shipment_titles(&html, &mut parcels)?;
    Ok(parcels.into_values().collect())
}

fn bind_shipment_titles(
    html: &Html,
    parcels: &mut BTreeMap<String, ParcelPage>,
) -> Result<(), Error> {
    // The grid row is the attested package boundary. Names elsewhere in the
    // order, including unshipped rows, cannot label its tracking candidates.
    let rows =
        selector(r#"[data-component="shipments"] > .a-box-group .a-fixed-right-grid-inner"#)?;
    let left_grid = selector(r#"[data-component="shipmentsLeftGrid"]"#)?;
    let right_grid = selector(r#"[data-component="shipmentsRightGrid"]"#)?;
    let connections = selector(r#"[data-component="shipmentConnections"] a[href]"#)?;
    let product_titles =
        selector(r#"[data-component="purchasedItems"] [data-component="itemTitle"] a"#)?;
    let mut bindings = BTreeMap::<(String, String, PackageSlot), Option<Vec<String>>>::new();
    for row in html.select(&rows) {
        let left: Vec<_> = row.select(&left_grid).collect();
        let right: Vec<_> = row.select(&right_grid).collect();
        let ([left], [right]) = (left.as_slice(), right.as_slice()) else {
            continue;
        };
        let mut packages = BTreeSet::new();
        let mut unbound = false;
        for anchor in right.select(&connections) {
            let Some(path) = anchor
                .value()
                .attr("href")
                .and_then(|href| allowed_path(href).ok())
                .filter(|path| is_tracking(path))
            else {
                continue;
            };
            let Some(parcel) = parcels.get(&path) else {
                unbound = true;
                break;
            };
            packages.insert((
                parcel.order.clone(),
                parcel.shipment.clone(),
                parcel.package.clone(),
            ));
        }
        if unbound || packages.len() != 1 {
            continue;
        }
        let Some(package) = packages.into_iter().next() else {
            continue;
        };
        let mut titles = Vec::new();
        for anchor in left.select(&product_titles) {
            let title = anchor
                .text()
                .flat_map(str::split_whitespace)
                .collect::<Vec<_>>()
                .join(" ");
            if !title.is_empty() && !title.chars().any(char::is_control) && !titles.contains(&title)
            {
                titles.push(title);
            }
        }
        if titles.is_empty() {
            continue;
        }
        match bindings.entry(package) {
            Entry::Vacant(entry) => {
                entry.insert(Some(titles));
            }
            // Duplicate presentation may agree; differing row contents cannot
            // be resolved by choosing whichever row appeared first.
            Entry::Occupied(mut entry) if entry.get().as_ref() != Some(&titles) => {
                entry.insert(None);
            }
            Entry::Occupied(_) => {}
        }
    }
    for parcel in parcels.values_mut() {
        let key = (
            parcel.order.clone(),
            parcel.shipment.clone(),
            parcel.package.clone(),
        );
        if let Some(Some(titles)) = bindings.get(&key) {
            parcel.titles.clone_from(titles);
        }
    }
    Ok(())
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
    #[serde(deserialize_with = "provider_optional")]
    promise: Option<Promise>,
    #[serde(deserialize_with = "provider_optional")]
    timezone: Option<String>,
}

#[derive(Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct Promise {
    #[serde(deserialize_with = "provider_optional")]
    promise_message: Option<String>,
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
    for header in html.select(&selector("h1, h2, h3, h4, h5, h6, [role=heading]")?) {
        let text = header
            .text()
            .flat_map(str::split_whitespace)
            .collect::<Vec<_>>()
            .join(" ")
            .to_ascii_uppercase();
        if let Some(carrier) = text
            .strip_prefix("DELIVERY BY ")
            .or_else(|| text.strip_prefix("DELIVERY FACILITATED BY "))
            .or_else(|| text.strip_prefix("SHIPPED WITH "))
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
    let observed_at = time::OffsetDateTime::now_utc();
    tracking_with_package(page, path, expected, None, observed_at)
}

pub(crate) fn tracking_parcel(
    page: &str,
    path: &str,
    parcel: &ParcelPage,
) -> Result<Tracker, Error> {
    if !query_identity(&parcel.path)?.compatible_with(&query_identity(path)?) {
        return Err(provider_error(ErrorKind::InvalidResponse));
    }
    let observed_at = time::OffsetDateTime::now_utc();
    tracking_with_package(page, path, None, parcel.package.package_id(), observed_at)
}

fn tracking_with_package(
    page: &str,
    path: &str,
    expected: Option<&str>,
    package: Option<&str>,
    observed_at: time::OffsetDateTime,
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
    let delivery_date = state
        .promise
        .and_then(|promise| promise.promise_message)
        .zip(state.timezone)
        .and_then(|(message, timezone)| {
            provider_delivery_date(&message, actual_status, observed_at, &timezone)
        });
    Ok(Tracker {
        provider: ProviderId::AmazonAccount,
        id: path.clone(),
        tracking_code: number,
        carrier: actual_carrier,
        status: actual_status,
        public_url: Some(format!("{ORIGIN}{path}")),
        carrier_tracking: CarrierTracking {
            delivery_date,
            ..Default::default()
        },
    })
}

fn provider_delivery_date(
    message: &str,
    status: Status,
    observed_at: time::OffsetDateTime,
    timezone: &str,
) -> Option<ProviderDeliveryDate> {
    if message.len() > 500 || message.chars().any(char::is_control) || timezone.len() > 128 {
        return None;
    }
    let zone = jiff::tz::TimeZoneDatabase::bundled().get(timezone).ok()?;
    let local = jiff::Timestamp::from_second(observed_at.unix_timestamp())
        .ok()?
        .to_zoned(zone)
        .date();
    let today = time::Date::from_calendar_date(
        i32::from(local.year()),
        time::Month::try_from(u8::try_from(local.month()).ok()?).ok()?,
        u8::try_from(local.day()).ok()?,
    )
    .ok()?;
    let (body, delivered) = if let Some(body) = message.strip_prefix("Delivered ") {
        if status != Status::Delivered {
            return None;
        }
        (body, true)
    } else {
        if Status::TERMINAL.contains(&status) {
            return None;
        }
        (
            message
                .strip_prefix("Arriving ")
                .or_else(|| message.strip_prefix("Now arriving "))?,
            false,
        )
    };
    let value = if let Some((start, end)) = body.split_once(" - ") {
        if delivered {
            return None;
        }
        let (start_month, start_day) = month_day(start)?;
        let (end_month, end_day) = month_day(end)?;
        let start = infer_year(start_month, start_day, today, false)?;
        let end_year = start.year() + i32::from(end_month < start_month);
        let end = time::Date::from_calendar_date(end_year, end_month, end_day).ok()?;
        if end < start {
            return None;
        }
        DeliveryDateValue::EstimatedRange { start, end }
    } else {
        let date = if body.eq_ignore_ascii_case("today") {
            today
        } else if body.eq_ignore_ascii_case("yesterday") {
            today.previous_day()?
        } else if body.eq_ignore_ascii_case("tomorrow") && !delivered {
            today.next_day()?
        } else {
            let (month, day) = month_day(body)?;
            infer_year(month, day, today, delivered)?
        };
        if delivered {
            DeliveryDateValue::Delivered { date }
        } else {
            DeliveryDateValue::Estimated { date }
        }
    };
    ProviderDeliveryDate::new(value, observed_at, timezone.into())
}

fn month_day(value: &str) -> Option<(time::Month, u8)> {
    let (month, day) = value.split_once(' ')?;
    let month = match month {
        "January" => time::Month::January,
        "February" => time::Month::February,
        "March" => time::Month::March,
        "April" => time::Month::April,
        "May" => time::Month::May,
        "June" => time::Month::June,
        "July" => time::Month::July,
        "August" => time::Month::August,
        "September" => time::Month::September,
        "October" => time::Month::October,
        "November" => time::Month::November,
        "December" => time::Month::December,
        _ => return None,
    };
    if day.is_empty() || day.len() > 2 || !day.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    Some((month, day.parse().ok()?))
}

fn infer_year(
    month: time::Month,
    day: u8,
    today: time::Date,
    delivered: bool,
) -> Option<time::Date> {
    let dates = [-1, 0, 1]
        .into_iter()
        .filter_map(|delta| time::Date::from_calendar_date(today.year() + delta, month, day).ok());
    if delivered {
        dates.filter(|date| *date <= today).max()
    } else {
        // Estimates may be overdue. Nearest adjacent-year date preserves that;
        // future wins a tie, without jumping years to find a leap day.
        dates.min_by_key(|date| ((*date - today).whole_days().abs(), *date < today))
    }
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

    fn promise_page(message: &str, short_status: &str, timezone_json: &str) -> String {
        let message = serde_json::to_string(message).unwrap();
        format!(
            r#"<h3>Delivery facilitated by Amazon</h3><script type="a-state" data-a-state='{{"key":"page-state"}}'>{{"trackingId":"TBA000000000070","shortStatus":"{short_status}","timezone":{timezone_json},"promise":{{"secondaryPromiseIdentifier":"DELIVERY","promiseMessage":{message}}}}}</script>"#
        )
    }

    #[test]
    fn provider_promise_produces_a_civil_date_without_inventing_an_actual_timestamp() {
        // Typed consumer projection also runs unchanged against the merge base.
        #[derive(serde::Deserialize)]
        struct DeliveryDate {
            kind: String,
            date: String,
            observed_at: String,
            timezone: String,
        }
        #[derive(serde::Deserialize)]
        struct TrackingDates {
            #[serde(default)]
            delivery_date: Option<DeliveryDate>,
            delivered_at: Option<String>,
            estimated_delivery_at: Option<String>,
            estimated_delivery_window: Option<crate::models::EstimatedDeliveryWindow>,
            events: Vec<crate::models::CarrierEvent>,
        }
        let html = promise_page(
            "Delivered October 25",
            "DELIVERED",
            r#""America/Los_Angeles""#,
        );
        let tracker = pages::tracking(&html, PATH, None).unwrap();
        let dates: TrackingDates =
            serde_json::from_str(&serde_json::to_string(&tracker.carrier_tracking).unwrap())
                .unwrap();
        let date = dates
            .delivery_date
            .expect("Provider civil delivery date was discarded");
        assert_eq!(date.kind, "delivered");
        assert!(date.date.ends_with("-10-25"));
        assert_eq!(date.date.len(), 10);
        assert_eq!(date.timezone, "America/Los_Angeles");
        time::OffsetDateTime::parse(
            &date.observed_at,
            &time::format_description::well_known::Rfc3339,
        )
        .unwrap();
        assert!(dates.delivered_at.is_none());
        assert!(dates.estimated_delivery_at.is_none());
        assert!(dates.estimated_delivery_window.is_none());
        assert!(dates.events.is_empty());
    }

    fn observed(value: &str) -> time::OffsetDateTime {
        time::OffsetDateTime::parse(value, &time::format_description::well_known::Rfc3339).unwrap()
    }

    fn civil(value: &str) -> time::Date {
        let format = time::format_description::parse_borrowed::<2>("[year]-[month]-[day]").unwrap();
        time::Date::parse(value, &format).unwrap()
    }

    #[test]
    fn yearless_provider_dates_resolve_rollover_overdue_and_leap_days() {
        use crate::models::DeliveryDateValue;
        for (message, status, at, expected) in [
            (
                "Delivered December 31",
                Status::Delivered,
                "2027-01-02T12:00:00Z",
                DeliveryDateValue::Delivered {
                    date: civil("2026-12-31"),
                },
            ),
            (
                "Delivered January 2",
                Status::Delivered,
                "2026-12-31T12:00:00Z",
                DeliveryDateValue::Delivered {
                    date: civil("2026-01-02"),
                },
            ),
            (
                "Arriving January 2",
                Status::InTransit,
                "2026-12-31T12:00:00Z",
                DeliveryDateValue::Estimated {
                    date: civil("2027-01-02"),
                },
            ),
            (
                "Arriving December 31",
                Status::InTransit,
                "2027-01-02T12:00:00Z",
                DeliveryDateValue::Estimated {
                    date: civil("2026-12-31"),
                },
            ),
            (
                "Arriving October 30",
                Status::InTransit,
                "2026-10-31T19:00:00Z",
                DeliveryDateValue::Estimated {
                    date: civil("2026-10-30"),
                },
            ),
            (
                "Delivered February 29",
                Status::Delivered,
                "2025-01-01T12:00:00Z",
                DeliveryDateValue::Delivered {
                    date: civil("2024-02-29"),
                },
            ),
            (
                "Arriving February 29",
                Status::InTransit,
                "2023-12-31T12:00:00Z",
                DeliveryDateValue::Estimated {
                    date: civil("2024-02-29"),
                },
            ),
            (
                "Now arriving December 31 - January 2",
                Status::InTransit,
                "2026-12-30T12:00:00Z",
                DeliveryDateValue::EstimatedRange {
                    start: civil("2026-12-31"),
                    end: civil("2027-01-02"),
                },
            ),
            (
                "Now arriving November 3 - November 8",
                Status::InTransit,
                "2026-10-31T19:00:00Z",
                DeliveryDateValue::EstimatedRange {
                    start: civil("2026-11-03"),
                    end: civil("2026-11-08"),
                },
            ),
        ] {
            let date =
                pages::provider_delivery_date(message, status, observed(at), "America/Los_Angeles")
                    .unwrap();
            assert_eq!(date.value, expected, "{message} at {at}");
            assert_eq!(date.observed_at, observed(at));
        }
    }

    #[test]
    fn relative_dates_use_provider_calendar_across_dst_not_utc_or_24_hours() {
        use crate::models::DeliveryDateValue;
        for (message, at, timezone, expected) in [
            // Spring-forward eve: UTC is already March 8; LA is still March 7.
            (
                "Arriving tomorrow",
                "2026-03-08T07:30:00Z",
                "America/Los_Angeles",
                "2026-03-08",
            ),
            // Spring-forward morning and fall-back duplicate local hour.
            (
                "Arriving tomorrow",
                "2026-03-08T09:30:00Z",
                "America/Los_Angeles",
                "2026-03-09",
            ),
            (
                "Arriving tomorrow",
                "2026-11-01T07:30:00Z",
                "America/Los_Angeles",
                "2026-11-02",
            ),
            (
                "Arriving tomorrow",
                "2026-11-01T08:30:00Z",
                "America/Los_Angeles",
                "2026-11-02",
            ),
            (
                "Arriving tomorrow",
                "2026-11-01T09:30:00Z",
                "America/Los_Angeles",
                "2026-11-02",
            ),
            (
                "Arriving today",
                "2027-01-01T07:30:00Z",
                "America/Los_Angeles",
                "2026-12-31",
            ),
            (
                "Arriving tomorrow",
                "2026-12-31T23:30:00Z",
                "Asia/Tokyo",
                "2027-01-02",
            ),
            (
                "Arriving tomorrow",
                "2024-02-28T20:00:00Z",
                "America/Los_Angeles",
                "2024-02-29",
            ),
        ] {
            let date =
                pages::provider_delivery_date(message, Status::InTransit, observed(at), timezone)
                    .unwrap();
            assert_eq!(
                date.value,
                DeliveryDateValue::Estimated {
                    date: civil(expected)
                }
            );
            assert_eq!(date.timezone, timezone);
        }
        let date = pages::provider_delivery_date(
            "Delivered today",
            Status::Delivered,
            observed("2027-01-01T07:30:00Z"),
            "America/Los_Angeles",
        )
        .unwrap();
        assert_eq!(
            date.value,
            DeliveryDateValue::Delivered {
                date: civil("2026-12-31")
            }
        );
        for (message, status, expected) in [
            (
                "Delivered yesterday",
                Status::Delivered,
                DeliveryDateValue::Delivered {
                    date: civil("2026-12-30"),
                },
            ),
            (
                "Arriving yesterday",
                Status::InTransit,
                DeliveryDateValue::Estimated {
                    date: civil("2026-12-30"),
                },
            ),
        ] {
            let date = pages::provider_delivery_date(
                message,
                status,
                observed("2027-01-01T07:30:00Z"),
                "America/Los_Angeles",
            )
            .unwrap();
            assert_eq!(date.value, expected);
        }
    }

    #[test]
    fn malformed_optional_promise_metadata_keeps_valid_tracking() {
        let valid = promise_page(
            "Delivered October 25",
            "DELIVERED",
            r#""America/Los_Angeles""#,
        );
        for html in [
            valid.replace(r#""promiseMessage":"Delivered October 25""#, r#""promiseMessage":17"#),
            valid.replace(r#""promiseMessage":"Delivered October 25""#, r#""promiseMessage":[]"#),
            valid.replace(r#""promiseMessage":"Delivered October 25""#, r#""promiseMessage":"""#),
            valid.replace(r#""promise":{"secondaryPromiseIdentifier":"DELIVERY","promiseMessage":"Delivered October 25"}"#, r#""promise":[]"#),
        ] {
            let tracker = pages::tracking(&html, PATH, None).unwrap();
            assert_eq!(tracker.status, Status::Delivered);
            assert!(tracker.carrier_tracking.delivery_date.is_none());
        }
        for timezone in [
            r#"17"#,
            r#"[]"#,
            r#""""#,
            r#""Pacific Standard Time""#,
            r#""Not/A_Real_Zone""#,
        ] {
            let html = promise_page("Delivered October 25", "DELIVERED", timezone);
            let tracker = pages::tracking(&html, PATH, None).unwrap();
            assert_eq!(tracker.status, Status::Delivered);
            assert!(tracker.carrier_tracking.delivery_date.is_none());
        }
        for (message, status) in [
            ("Delivered tomorrow", Status::Delivered),
            ("Arriving October 25", Status::Delivered),
            ("Delivered October 25", Status::InTransit),
            ("Arriving February 30", Status::InTransit),
            ("Arriving February 29", Status::InTransit),
            ("Now arriving November 8 - November 3", Status::InTransit),
            ("Arriving sometime soon", Status::InTransit),
            ("Arriving tomorrow by 10 PM", Status::InTransit),
            ("Arriving October 25 2026", Status::InTransit),
            ("Delivered October 25\nPRIVATE", Status::Delivered),
        ] {
            assert!(
                pages::provider_delivery_date(
                    message,
                    status,
                    observed("2026-10-31T19:00:00Z"),
                    "America/Los_Angeles"
                )
                .is_none(),
                "{message}"
            );
        }
    }

    #[test]
    fn listing_keeps_first_seen_dom_order_while_grouping_package_items() {
        let recent =
            "/progress-tracker/package?orderId=999-1234567-1234567&shipmentId=SHIP2&packageIndex=0";
        let older =
            "/progress-tracker/package?orderId=111-1234567-1234567&shipmentId=SHIP1&packageIndex=0";
        let listing = pages::listing(&format!(
            r#"<a class="item-card__link" href="{recent}" aria-label="Recent adapter, Order placed 31 October 2026, Shipped"></a><a class="item-card__link" href="{older}" aria-label="Older book, Order placed 23 October 2026, Shipped"></a><a class="item-card__link" href="{recent}" aria-label="Recent cable, Order placed 31 October 2026, Shipped"></a>"#
        )).unwrap();
        assert_eq!(listing.parcels.len(), 2);
        assert_eq!(listing.parcels[0].path, recent);
        assert_eq!(
            listing.parcels[0].titles,
            ["Recent adapter", "Recent cable"]
        );
        assert_eq!(listing.parcels[1].path, older);
        assert_eq!(listing.parcels[1].titles, ["Older book"]);
    }

    #[test]
    fn listing_pagination_normalizes_only_bounded_recent_order_parameters() {
        assert_eq!(
            pages::allowed_path("/your-orders/orders?page=1&ref_=fixture&timeFilter=months-3")
                .unwrap(),
            "/your-orders/orders?page=1&timeFilter=months-3",
        );
        for page in [0, 100] {
            let path = format!("/your-orders/orders?page={page}");
            assert_eq!(pages::allowed_path(&path).unwrap(), path);
        }
        assert_eq!(
            pages::allowed_path("/your-orders/orders?page=001").unwrap(),
            "/your-orders/orders?page=1"
        );
        for query in [
            "page=101",
            "page=-1",
            "page=1.0",
            "page=+1",
            "page=",
            "page=PRIVATE",
            "page=1&page=2",
            "page=1&PAGE=2",
            "timeFilter=year-2025",
            "timeFilter=",
            "timeFilter=months-3&timeFilter=months-3",
        ] {
            assert!(
                pages::allowed_path(&format!("/your-orders/orders?{query}")).is_err(),
                "{query}"
            );
        }
        assert_eq!(
            pages::allowed_path(&format!("{PATH}&page=1&timeFilter=months-3")).unwrap(),
            PATH
        );
    }

    #[test]
    fn mobile_listing_parameters_preserve_the_actual_start_index() {
        assert_eq!(
            pages::allowed_path(
                "/your-orders/orders?startIndex=10&timeFilter=months-3&ref_=fixture"
            )
            .unwrap(),
            "/your-orders/orders?startIndex=10&timeFilter=months-3"
        );
    }

    #[test]
    fn mobile_chunk_pagination_keeps_only_the_validated_recent_order_ordinal() {
        let item = format!(
            r#"<a class="item-card__link" href="{PATH}" aria-label="Widget, Order placed 1 October 2026, Shipped"></a>"#
        );
        let first = format!(
            r#"{item}<script type="a-state" data-a-state='{{"key":"chunkState"}}'>{{"nextStartIndex":10,"timeFilter":"months-3","nextContinuationToken":"unused-fixture"}}</script>"#
        );
        assert_eq!(
            pages::listing(&first).unwrap().next_page.as_deref(),
            Some("/your-orders/orders?startIndex=10&timeFilter=months-3")
        );
        let second = first.replace("\"nextStartIndex\":10", "\"nextStartIndex\":20");
        assert_eq!(
            pages::listing(&second).unwrap().next_page.as_deref(),
            Some("/your-orders/orders?startIndex=20&timeFilter=months-3")
        );
        assert!(pages::listing(&item).unwrap().next_page.is_none());
        assert!(
            pages::listing(&first.replace("\"nextStartIndex\":10", "\"nextStartIndex\":null"))
                .unwrap()
                .next_page
                .is_none()
        );
        for bad in ["0", "11", "1010", "-10", "\"20\""] {
            let html = first.replace(
                "\"nextStartIndex\":10",
                &format!("\"nextStartIndex\":{bad}"),
            );
            assert!(pages::listing(&html).is_err(), "{bad}");
        }
        assert!(pages::listing(&first.replace("months-3", "year-2025")).is_err());
        assert!(pages::listing(&(first.clone() + &second)).is_err());
        for (query, ordinal) in [
            ("startIndex=0", 0),
            ("startIndex=10", 1),
            ("startIndex=1000", 100),
            ("page=2", 2),
            ("timeFilter=months-3", 0),
        ] {
            let path = format!("/your-orders/orders?{query}");
            assert_eq!(pages::listing_page_index(&path), Some(ordinal));
            assert_eq!(pages::allowed_path(&path).unwrap(), path);
        }
        for query in [
            "startIndex=11",
            "startIndex=1010",
            "startIndex=-10",
            "startIndex=",
            "startIndex=10&STARTINDEX=20",
            "page=1&startIndex=10",
        ] {
            let path = format!("/your-orders/orders?{query}");
            assert!(pages::allowed_path(&path).is_err(), "{query}");
            assert!(pages::listing_page_index(&path).is_none());
        }
        assert!(pages::same_parcel_identity(
            "/your-orders/orders?startIndex=10&timeFilter=months-3",
            "/your-orders/orders?page=1&timeFilter=months-3"
        ));
        assert!(!pages::same_parcel_identity(
            "/your-orders/orders?startIndex=10&timeFilter=months-3",
            "/your-orders/orders?startIndex=20&timeFilter=months-3"
        ));
        assert!(!pages::same_parcel_identity(
            "/your-orders/orders?startIndex=10&timeFilter=months-3",
            "/your-orders/orders?startIndex=10"
        ));
    }

    #[test]
    fn listing_extracts_one_real_next_page_and_deduplicates_equivalent_links() {
        let item = format!(
            r#"<a class="item-card__link" href="{PATH}" aria-label="Widget, Order placed 1 October 2026, Shipped"></a>"#
        );
        let listing = pages::listing(&(item.clone() + r#"<ul class="a-pagination"><li class="a-last"><a href="/your-orders/orders?page=1&timeFilter=months-3&ref_=fixture">Next→</a></li></ul><a rel="next" href="/your-orders/orders?timeFilter=months-3&page=1&ref_=other">Next</a>"#)).unwrap();
        assert_eq!(
            listing.next_page.as_deref(),
            Some("/your-orders/orders?page=1&timeFilter=months-3")
        );
        assert_eq!(listing.parcels.len(), 1);
        let last = pages::listing(&(item + r#"<ul class="a-pagination"><li class="a-disabled a-last"><span>Next→</span></li></ul>"#)).unwrap();
        assert!(last.next_page.is_none());
        let empty = pages::listing("<main>You have no orders</main>").unwrap();
        assert!(empty.next_page.is_none());
    }

    #[test]
    fn listing_next_links_never_leave_recent_order_listing_routes() {
        let item = format!(
            r#"<a class="item-card__link" href="{PATH}" aria-label="Widget, Order placed 1 October 2026, Shipped"></a>"#
        );
        for href in [
            "https://evil.example/your-orders/orders?page=1",
            "/your-orders/pop?orderId=111-1234567-1234567&page=1",
            "/progress-tracker/package?orderId=111-1234567-1234567&shipmentId=SHIP1&packageIndex=0&page=1",
            "/your-orders/orders",
            "/your-orders/orders?page=101",
            "/your-orders/orders?page=1&page=2",
            "/your-orders/orders?page=1&timeFilter=year-2025",
        ] {
            let html = format!(r#"{item}<a rel="next" href="{href}">Next</a>"#);
            assert!(pages::listing(&html).is_err(), "{href}");
        }
        let ambiguous = format!(
            r#"{item}<a rel="next" href="/your-orders/orders?page=1">Next</a><a rel="next" href="/your-orders/orders?page=2">Next</a>"#
        );
        assert!(pages::listing(&ambiguous).is_err());
    }

    #[test]
    fn listing_redirects_retain_page_and_any_explicit_recent_filter() {
        let original = "/your-orders/orders?page=1&timeFilter=months-3";
        let same = "/-/en/gp/css/order-history?page=1&timeFilter=months-3";
        assert!(pages::same_parcel_identity(original, same));
        for destination in [
            "/your-orders/orders?page=2&timeFilter=months-3",
            "/your-orders/orders?timeFilter=months-3",
            "/your-orders/orders?page=1",
            "https://evil.example/your-orders/orders?page=1&timeFilter=months-3",
            "/your-orders/order-details?orderID=111-1234567-1234567",
        ] {
            assert!(
                !pages::same_parcel_identity(original, destination),
                "{destination}"
            );
        }
        assert!(pages::same_parcel_identity(
            "/your-orders/orders",
            "/gp/css/order-history?page=0"
        ));
        assert!(!pages::same_parcel_identity(
            "/your-orders/orders",
            "/gp/css/order-history?page=1"
        ));
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
    fn checkpoint_forms_require_same_origin_validation_and_a_checkbox() {
        for action in [
            "/checkpoint/validate",
            "https://www.amazon.com/checkpoint/validate",
        ] {
            let page = format!(
                r#"<form action="{action}"><input type="hidden" name="opaque" value="PRIVATE_CHECKPOINT_FIELD"><label><input type="checkbox" name="human">Check the box to continue. I am human.</label></form>"#
            );
            assert_eq!(pages::orders_shape(&page), "challenge");
            assert_eq!(
                pages::order_details_failure_shape(&page, "111-1234567-1234567"),
                "challenge"
            );
        }
        for page in [
            r#"<form action="https://evil.example/checkpoint/validate"><input type="checkbox"></form>"#,
            r#"<form action="http://www.amazon.com/checkpoint/validate"><input type="checkbox"></form>"#,
            r#"<form action="/checkpoint/not-validation"><input type="checkbox"></form>"#,
            r#"<form action="/checkpoint/validate"><input type="hidden" value="PRIVATE_CHECKPOINT_FIELD"></form>"#,
            r#"<p>Service unavailable. /checkpoint/validate Check the box to continue.</p>"#,
        ] {
            assert_ne!(pages::orders_shape(page), "challenge");
        }
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
    fn semantic_shipping_headings_verify_explicit_carriers_without_carrier_code() {
        for (heading, number, expected) in [
            (
                "<h1>Delivery\n by   Amazon</h1>",
                "TBA000000000020",
                Carrier::AmazonAccount,
            ),
            (
                "<h4>Shipped with <span>Amazon</span></h4>",
                "TBA000000000020",
                Carrier::AmazonAccount,
            ),
            (
                "<div role=heading>delivery facilitated BY USPS</div>",
                "9400111122223333444455",
                Carrier::Usps,
            ),
            (
                "<h6>shipped WITH FedEx</h6>",
                "781234567890",
                Carrier::FedExDefault,
            ),
        ] {
            let html = page(number).replace("<h3>Delivery By USPS</h3>", heading);
            let tracker = pages::tracking(&html, PATH, Some(number)).unwrap();
            assert_eq!(tracker.carrier, expected);
            assert_eq!(tracker.tracking_code, number);
            assert_eq!(tracker.status, Status::InTransit);
        }
    }

    #[test]
    fn shipping_headings_do_not_guess_carriers_or_override_conflicting_evidence() {
        for heading in [
            "<h1>Shipped with Unknown carrier</h1>",
            "<p>Shipped with Amazon</p>",
            "<h4>Shipped with Amazon</h4><h6>Delivery by USPS</h6>",
        ] {
            let html = page("TBA000000000020").replace("<h3>Delivery By USPS</h3>", heading);
            assert!(pages::tracking(&html, PATH, None).is_err());
        }
        let conflict = page("9400111122223333444455")
            .replace("<h3>Delivery By USPS</h3>", "<h4>Shipped with Amazon</h4>")
            .replace(
                "\"trackingId\":",
                "\"carrierCode\":\"USPS\",\"trackingId\":",
            );
        assert!(pages::tracking(&conflict, PATH, None).is_err());
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
    fn canonical_shipment_titles_stay_inside_one_uniquely_bound_grid_row() {
        let order = "111-1234567-1234567";
        let path = |shipment: &str, index: u8| {
            format!(
                "/progress-tracker/package?orderId={order}&shipmentId={shipment}&packageIndex={index}"
            )
        };
        let first = path("SHIP1", 0);
        let sibling = path("SHIP2", 0);
        let unknown = path("SHIP3", 0);
        let multiple_left = path("SHIP4", 0);
        let shared_first = path("SHIP5", 0);
        let shared_second = path("SHIP5", 1);
        let html = format!(
            r#"
            <div data-component="itemTitle"><a>Whole-order title trap</a></div>
            <a href="{unknown}">Unstructured package</a>
            <div data-component="shipments"><div class="a-box-group">
              <div class="a-fixed-right-grid-inner">
                <div data-component="shipmentsLeftGrid"><div data-component="purchasedItems">
                  <div data-component="itemTitle"><a>Adapter &amp; Cable</a></div>
                  <div data-component="itemTitle"><a>Spare bracket</a></div>
                </div></div>
                <div data-component="shipmentsRightGrid"><div data-component="shipmentConnections">
                  <a href="{first}">Track</a><a href="{first}&lineItemId=ITEM2">Same package</a>
                </div></div>
              </div>
              <div class="a-fixed-right-grid-inner">
                <div data-component="shipmentsLeftGrid"><div data-component="purchasedItems"><div data-component="itemTitle"><a>Sibling lamp</a></div></div></div>
                <div data-component="shipmentsRightGrid"><div data-component="shipmentConnections"><a href="{sibling}">Track</a></div></div>
              </div>
              <div class="a-fixed-right-grid-inner">
                <div data-component="shipmentsLeftGrid"><div data-component="purchasedItems"><div data-component="itemTitle"><a>Unshipped monitor</a></div></div></div>
                <div data-component="shipmentsRightGrid">Not shipped</div>
              </div>
              <div class="a-fixed-right-grid-inner">
                <div data-component="shipmentsLeftGrid"><div data-component="purchasedItems"><div data-component="itemTitle"><a>Ambiguous first item</a></div></div></div>
                <div data-component="shipmentsLeftGrid"><div data-component="purchasedItems"><div data-component="itemTitle"><a>Ambiguous second item</a></div></div></div>
                <div data-component="shipmentsRightGrid"><div data-component="shipmentConnections"><a href="{multiple_left}">Track</a></div></div>
              </div>
              <div class="a-fixed-right-grid-inner">
                <div data-component="shipmentsLeftGrid"><div data-component="purchasedItems"><div data-component="itemTitle"><a>Do not copy onto two boxes</a></div></div></div>
                <div data-component="shipmentsRightGrid"><div data-component="shipmentConnections"><a href="{shared_first}">Track</a><a href="{shared_second}">Track another</a></div></div>
              </div>
            </div></div>"#
        );
        let parcels = pages::order_tracking_pages(&html, order).unwrap();
        let titles = |path: &str| {
            &parcels
                .iter()
                .find(|parcel| parcel.path == path)
                .unwrap()
                .titles
        };
        assert_eq!(titles(&first), &["Adapter & Cable", "Spare bracket"]);
        assert_eq!(titles(&format!("{first}&lineItemId=ITEM2")), titles(&first));
        assert_eq!(titles(&sibling), &["Sibling lamp"]);
        for path in [unknown, multiple_left, shared_first, shared_second] {
            assert!(titles(&path).is_empty());
        }
    }

    #[test]
    fn canonical_shipment_titles_do_not_choose_between_conflicting_rows() {
        let order = "111-1234567-1234567";
        let path =
            format!("/progress-tracker/package?orderId={order}&shipmentId=SHIP1&packageIndex=0");
        let rows: String = ["First row product", "Conflicting row product"].iter().map(|name| format!(r#"<div class="a-fixed-right-grid-inner"><div data-component="shipmentsLeftGrid"><div data-component="purchasedItems"><div data-component="itemTitle"><a>{name}</a></div></div></div><div data-component="shipmentsRightGrid"><div data-component="shipmentConnections"><a href="{path}">Track</a></div></div></div>"#)).collect();
        let html = format!(
            r#"<div data-component="shipments"><div class="a-box-group">{rows}</div></div>"#
        );
        let parcels = pages::order_tracking_pages(&html, order).unwrap();
        assert_eq!(parcels.len(), 1);
        assert!(parcels[0].titles.is_empty());
    }

    #[test]
    fn canonical_identity_sets_reject_conflicts_but_preserve_consistent_variants() {
        let order = "111-1234567-1234567";
        let first = format!(
            "/progress-tracker/package?orderId={order}&shipmentId=SHIP1&packageId=BOX1&packageIndex=0"
        );
        for second in [
            first.replace("packageIndex=0", "packageIndex=1"),
            first.replace("packageId=BOX1", "packageId=BOX2"),
        ] {
            let html =
                format!(r#"<a href="{first}">Track</a><a href="{second}">Conflicting package</a>"#);
            assert!(
                pages::order_tracking_pages(&html, order).is_err(),
                "contradictory canonical package identities must not pick one URL"
            );
        }
        for second in [
            format!("{first}&lineItemId=ITEM2"),
            first.replace(
                "packageId=BOX1&packageIndex=0",
                "packageId=BOX2&packageIndex=1",
            ),
            first.replace(
                "shipmentId=SHIP1&packageId=BOX1",
                "shipmentId=SHIP2&packageId=BOX2",
            ),
            first.replace("&packageId=BOX1", ""),
        ] {
            let html =
                format!(r#"<a href="{first}">Track</a><a href="{second}">Consistent variant</a>"#);
            assert_eq!(pages::order_tracking_pages(&html, order).unwrap().len(), 2);
        }
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
