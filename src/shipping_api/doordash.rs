//! Read-only recipient tracking. The original link is a capability, not a
//! carrier credential; it must never appear in logs or request errors.

use std::{collections::BTreeMap, net::IpAddr, sync::Arc, time::Duration};

use reqwest::{Client, Response, Url};
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::{Mutex, OwnedMutexGuard};
use tokio::time::Instant;
use uuid::Uuid;

use crate::{
    carrier::Carrier,
    models::{CarrierTracking, Status, provider_optional},
    shipping_api::{
        PreparedRegistration, ShippingApi,
        error::{Error, ErrorKind, Rejection},
        types::{ApiFuture, ProviderId, Registration, Tracker},
    },
};

const ORIGIN: &str = "https://www.doordash.com";
const RECIPIENT_PATH: &str = "/orders/drive";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
const MAX_BODY_BYTES: usize = 4 * 1024 * 1024;
const MAX_RETRY_SECONDS: i64 = 6 * 60 * 60;
const MAX_CODE_BYTES: usize = 128;
const MAX_FLIGHT_RECORDS: usize = 4096;

fn provider_error(kind: ErrorKind) -> Error {
    Error::new(ProviderId::DoorDash, kind)
}

fn invalid_link() -> Error {
    provider_error(ErrorKind::Rejected(Rejection::InvalidRecipientLink))
}

fn valid_code(code: &str) -> bool {
    !code.is_empty()
        && code.len() <= MAX_CODE_BYTES
        && code
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

/// Only the original recipient route is accepted. Canonicalization never
/// follows a link, resolves an arbitrary host, or turns a barcode into a URL.
pub fn normalize_recipient_url(input: &str) -> Result<String, Error> {
    if input.len() > 2048 || input.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(invalid_link());
    }
    let url = Url::parse(input).map_err(|_| invalid_link())?;
    if url.scheme() != "https"
        || url.host_str() != Some("www.doordash.com")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some_and(|port| port != 443)
        || url.path() != RECIPIENT_PATH
        || url.fragment().is_some()
        || input
            .split_once("://")
            .and_then(|(_, rest)| rest.split('/').next())
            .is_some_and(|authority| authority.contains('@') || authority.contains('\\'))
    {
        return Err(invalid_link());
    }
    let mut pairs = url.query_pairs();
    let Some((name, code)) = pairs.next() else {
        return Err(invalid_link());
    };
    if name != "urlCode" || !valid_code(&code) || pairs.next().is_some() {
        return Err(invalid_link());
    }
    Ok(recipient_url(&code))
}

fn recipient_url(code: &str) -> String {
    format!("{ORIGIN}{RECIPIENT_PATH}?urlCode={code}")
}

fn registration_code(registration: &Registration) -> Result<String, Error> {
    if registration
        .carrier
        .is_some_and(|carrier| carrier != Carrier::DoorDash)
    {
        return Err(provider_error(ErrorKind::Rejected(
            Rejection::CarrierCodeMismatch,
        )));
    }
    let input = registration
        .recipient_url
        .as_deref()
        .ok_or_else(|| provider_error(ErrorKind::Rejected(Rejection::RecipientLinkRequired)))?;
    let normalized = normalize_recipient_url(input)?;
    normalized
        .strip_prefix(&format!("{ORIGIN}{RECIPIENT_PATH}?urlCode="))
        .map(str::to_owned)
        .ok_or_else(invalid_link)
}

#[derive(Clone)]
pub struct DoorDashClient {
    client: Client,
    requests: Arc<Mutex<RequestSchedule>>,
    log: slog::Logger,
    request_id: Option<String>,
    loopback: Option<Url>,
}

struct RequestSchedule {
    next_start: Instant,
    cooldown: Option<Instant>,
    rate_limits: u32,
}

impl DoorDashClient {
    pub fn new() -> Result<Self, Error> {
        Ok(Self::from_client(
            Client::builder()
                .timeout(REQUEST_TIMEOUT)
                .redirect(reqwest::redirect::Policy::none())
                .user_agent(concat!("shipments/", env!("CARGO_PKG_VERSION")))
                .build()
                .map_err(|_| provider_error(ErrorKind::Configuration))?,
        ))
    }

    /// A literal loopback address is the only configurable destination. User
    /// links still pass the production allowlist before reaching this fixture.
    pub fn for_loopback(base_url: &str) -> Result<Self, Error> {
        let url = Url::parse(base_url).map_err(|_| provider_error(ErrorKind::Configuration))?;
        let loopback = url
            .host_str()
            .and_then(|host| host.trim_matches(['[', ']']).parse::<IpAddr>().ok())
            .is_some_and(|address| address.is_loopback());
        if !loopback
            || !matches!(url.scheme(), "http" | "https")
            || !url.username().is_empty()
            || url.password().is_some()
            || url.path() != "/"
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(provider_error(ErrorKind::Configuration));
        }
        let mut client = Self::new()?;
        client.loopback = Some(url);
        Ok(client)
    }

    fn from_client(client: Client) -> Self {
        Self {
            client,
            requests: Arc::new(Mutex::new(RequestSchedule {
                next_start: Instant::now(),
                cooldown: None,
                rate_limits: 0,
            })),
            log: crate::logging::logger(),
            request_id: None,
            loopback: None,
        }
    }

    pub fn with_logger(mut self, log: slog::Logger) -> Self {
        self.log = log;
        self
    }

    async fn observe<T>(
        &self,
        operation: &'static str,
        future: impl std::future::Future<Output = Result<T, Error>>,
    ) -> Result<T, Error> {
        let started = Instant::now();
        let result = future.await;
        let error = result.as_ref().err();
        let outcome = match error.map(|error| &error.kind) {
            None => "success",
            Some(ErrorKind::Throttled { .. }) => "throttled",
            Some(ErrorKind::Http { status: 429, .. }) => "rate_limited",
            Some(ErrorKind::Rejected(_)) => "rejected",
            _ => "error",
        };
        let status = error.and_then(Error::provider_status).or_else(|| {
            match error.map(|error| &error.kind) {
                None
                | Some(
                    ErrorKind::RecipientStatusUnavailable
                    | ErrorKind::ResponseTooLarge
                    | ErrorKind::Rejected(Rejection::RecipientLinkMismatch),
                ) => Some(200_u16),
                _ => None,
            }
        });
        slog::info!(self.log, "provider_request";
            "request_id" => self.request_id.clone().unwrap_or_else(|| Uuid::now_v7().to_string()),
            "provider" => ProviderId::DoorDash.as_str(),
            "operation" => operation,
            "outcome" => outcome,
            "duration_ms" => started.elapsed().as_millis(),
            "provider_status" => status,
            "error_code" => error.map(Error::error_code),
            "retry_after_seconds" => error.and_then(Error::retry_after));
        result
    }

    async fn request_slot(&self) -> Result<OwnedMutexGuard<RequestSchedule>, Error> {
        let schedule = self
            .requests
            .clone()
            .try_lock_owned()
            .map_err(|_| provider_error(ErrorKind::Throttled { retry_after: 1 }))?;
        if let Some(until) = schedule.cooldown.filter(|until| *until > Instant::now()) {
            return Err(provider_error(ErrorKind::Throttled {
                retry_after: i64::try_from(
                    until.saturating_duration_since(Instant::now()).as_secs(),
                )
                .unwrap_or(MAX_RETRY_SECONDS)
                .saturating_add(1)
                .clamp(1, MAX_RETRY_SECONDS),
            }));
        }
        tokio::time::sleep_until(schedule.next_start).await;
        Ok(schedule)
    }

    fn blocked(&self) -> bool {
        self.requests.try_lock().map_or(true, |schedule| {
            schedule
                .cooldown
                .is_some_and(|until| until > Instant::now())
        })
    }

    async fn fetch(&self, code: &str, tracking_number: &str) -> Result<Tracker, Error> {
        self.fetch_in_slot(code, tracking_number, self.request_slot().await?)
            .await
    }

    async fn fetch_in_slot(
        &self,
        code: &str,
        tracking_number: &str,
        mut schedule: OwnedMutexGuard<RequestSchedule>,
    ) -> Result<Tracker, Error> {
        if !valid_code(code) {
            return Err(invalid_link());
        }
        schedule.next_start = Instant::now() + Duration::from_secs(1);
        let destination = recipient_url(code);
        let destination = if let Some(base) = &self.loopback {
            base.join(&format!("{RECIPIENT_PATH}?urlCode={code}"))
                .map_err(|_| provider_error(ErrorKind::Configuration))?
                .to_string()
        } else {
            destination
        };
        let result = async {
            let response = self
                .client
                .get(destination)
                .header(reqwest::header::ACCEPT, "text/html")
                .send()
                .await
                .map_err(redact_request_error)?;
            let body = read_body(response).await?;
            parse_page(&body, code, tracking_number)
        }
        .await;
        match result {
            Err(mut error) if error.provider_status() == Some(429) => {
                schedule.rate_limits = schedule.rate_limits.saturating_add(1);
                let delay = 60_i64
                    .saturating_mul(2_i64.saturating_pow(schedule.rate_limits.saturating_sub(1)))
                    .clamp(60, MAX_RETRY_SECONDS)
                    .max(error.retry_after().unwrap_or(0));
                schedule.cooldown =
                    Some(Instant::now() + Duration::from_secs(delay.unsigned_abs()));
                error.set_retry_after(delay);
                Err(error)
            }
            Err(error) if error.retry_after().is_some() => {
                if let Some(delay) = error.retry_after() {
                    schedule.cooldown =
                        Some(Instant::now() + Duration::from_secs(delay.unsigned_abs()));
                }
                Err(error)
            }
            Ok(tracker) => {
                schedule.rate_limits = 0;
                schedule.cooldown = None;
                Ok(tracker)
            }
            Err(error) => Err(error),
        }
    }
}

fn redact_request_error(error: reqwest::Error) -> Error {
    provider_error(if error.is_timeout() {
        ErrorKind::Timeout
    } else {
        ErrorKind::Connection
    })
}

async fn read_body(mut response: Response) -> Result<Vec<u8>, Error> {
    let status = response.status();
    if status.as_u16() != 200 {
        let retry_after = response
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<i64>().ok())
            .filter(|seconds| (1..=MAX_RETRY_SECONDS).contains(seconds));
        // Error pages are never parsed, retained or interpolated into errors.
        return Err(provider_error(ErrorKind::Http {
            status: status.as_u16(),
            retry_after,
        }));
    }
    if !response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.split(';').next() == Some("text/html"))
    {
        return Err(provider_error(ErrorKind::RecipientStatusUnavailable));
    }
    if response
        .content_length()
        .is_some_and(|length| length > MAX_BODY_BYTES as u64)
    {
        return Err(provider_error(ErrorKind::ResponseTooLarge));
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(redact_request_error)? {
        if chunk.len() > MAX_BODY_BYTES.saturating_sub(body.len()) {
            return Err(provider_error(ErrorKind::ResponseTooLarge));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

#[derive(Deserialize)]
#[serde(untagged)]
enum FlightChunk {
    Text((u8, String)),
    Bootstrap((u8,)),
    FormState((u8, serde::de::IgnoredAny)),
}

/// The React transport is the dynamic boundary. Only the selected tracking
/// payload crosses into owned types; HTML, JS and provider text are not run.
fn flight_records(html: &str) -> Result<BTreeMap<String, Value>, Error> {
    let unavailable = || provider_error(ErrorKind::RecipientStatusUnavailable);
    let lowercase = html.to_ascii_lowercase();
    let mut remaining = 0;
    let mut flight = String::new();
    let mut chunks = 0;
    while let Some(start) = lowercase[remaining..].find("<script") {
        let start = remaining + start + "<script".len();
        if !lowercase
            .as_bytes()
            .get(start)
            .is_some_and(|byte| byte.is_ascii_whitespace() || matches!(byte, b'>' | b'/'))
        {
            remaining = start;
            continue;
        }
        let body_start = start + lowercase[start..].find('>').ok_or_else(unavailable)? + 1;
        let body_end = body_start
            + lowercase[body_start..]
                .find("</script>")
                .ok_or_else(unavailable)?;
        let script = &html[body_start..body_end];
        for (index, marker) in script.match_indices("self.__next_f.push(") {
            let after = &script[index + marker.len()..];
            let chunk = serde_json::Deserializer::from_str(after)
                .into_iter::<FlightChunk>()
                .next()
                .ok_or_else(unavailable)?
                .map_err(|_| unavailable())?;
            match chunk {
                FlightChunk::Text((1, value)) => {
                    chunks += 1;
                    if chunks > 512 || value.len() > MAX_BODY_BYTES.saturating_sub(flight.len()) {
                        return Err(unavailable());
                    }
                    flight.push_str(&value);
                }
                FlightChunk::Bootstrap((0,)) => {}
                FlightChunk::FormState((2, _)) => {}
                _ => return Err(unavailable()),
            }
        }
        remaining = body_end + "</script>".len();
    }
    let mut records = BTreeMap::new();
    let mut rest = flight.as_str();
    while !rest.is_empty() {
        let (line, tail) = rest.split_once('\n').unwrap_or((rest, ""));
        let Some((id, payload)) = line.split_once(':') else {
            return Err(unavailable());
        };
        if id.is_empty() || id.len() > 16 || !id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(unavailable());
        }
        // Length-prefixed text may itself contain newlines and apparent record
        // headers. Skip its declared bytes rather than interpreting that text.
        if let Some(text) = payload.strip_prefix('T') {
            let (hex_length, _) = text.split_once(',').ok_or_else(unavailable)?;
            let length = usize::from_str_radix(hex_length, 16).map_err(|_| unavailable())?;
            let header = id.len() + 3 + hex_length.len();
            rest = rest
                .get(header..)
                .and_then(|rest| rest.get(length..))
                .ok_or_else(unavailable)?;
            rest = rest.strip_prefix('\n').unwrap_or(rest);
            continue;
        }
        if payload.as_bytes().first().is_some_and(|byte| {
            byte.is_ascii_digit() || matches!(byte, b'[' | b'{' | b'"' | b'n' | b't' | b'f' | b'-')
        }) {
            let value = serde_json::from_str(payload).map_err(|_| unavailable())?;
            if records.len() >= MAX_FLIGHT_RECORDS || records.insert(id.into(), value).is_some() {
                return Err(unavailable());
            }
        }
        rest = tail;
    }
    Ok(records)
}

fn resolve_references(
    value: &Value,
    records: &BTreeMap<String, Value>,
    depth: usize,
    remaining_nodes: &mut usize,
) -> Result<Value, Error> {
    let unavailable = || provider_error(ErrorKind::RecipientStatusUnavailable);
    if depth >= 64 || *remaining_nodes == 0 {
        return Err(unavailable());
    }
    *remaining_nodes -= 1;
    match value {
        Value::String(value) if value == "$undefined" => Ok(Value::Null),
        Value::String(value)
            if value
                .strip_prefix('$')
                .is_some_and(|id| !id.is_empty() && id.bytes().all(|b| b.is_ascii_hexdigit())) =>
        {
            let referenced = records.get(&value[1..]).ok_or_else(unavailable)?;
            resolve_references(referenced, records, depth + 1, remaining_nodes)
        }
        Value::Array(values) => values
            .iter()
            .map(|value| resolve_references(value, records, depth + 1, remaining_nodes))
            .collect::<Result<Vec<_>, _>>()
            .map(Value::Array),
        Value::Object(values) => values
            .iter()
            .map(|(key, value)| {
                Ok((
                    key.clone(),
                    resolve_references(value, records, depth + 1, remaining_nodes)?,
                ))
            })
            .collect::<Result<serde_json::Map<_, _>, Error>>()
            .map(Value::Object),
        _ => Ok(value.clone()),
    }
}

fn find_tracking_payloads<'a>(
    value: &'a Value,
    depth: usize,
    remaining_nodes: &mut usize,
    found: &mut Vec<&'a Value>,
) -> Result<(), Error> {
    if depth >= 64 || *remaining_nodes == 0 || found.len() > 1 {
        return Err(provider_error(ErrorKind::RecipientStatusUnavailable));
    }
    *remaining_nodes -= 1;
    match value {
        Value::Object(values) => {
            if values.contains_key("externalDeliveryId")
                && values.contains_key("getOrderStatusResponse")
            {
                found.push(value);
            } else {
                for value in values.values() {
                    find_tracking_payloads(value, depth + 1, remaining_nodes, found)?;
                }
            }
        }
        Value::Array(values) => {
            for value in values {
                find_tracking_payloads(value, depth + 1, remaining_nodes, found)?;
            }
        }
        _ => {}
    }
    Ok(())
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RecipientTracking {
    external_delivery_id: String,
    get_order_status_response: OrderStatusResponse,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct OrderStatusResponse {
    merchant_order_status: Vec<MerchantOrder>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct MerchantOrder {
    order_details: OrderDetails,
    #[serde(default, deserialize_with = "provider_optional")]
    eta_details: Option<EtaDetails>,
    #[serde(default, deserialize_with = "provider_optional")]
    progress_bar: Option<ProgressBar>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct OrderDetails {
    order_status: OrderStatus,
}

#[derive(Clone, Copy, Deserialize)]
enum OrderStatus {
    #[serde(rename = "delivered_order", alias = "delivered")]
    Delivered,
    #[serde(
        rename = "cancelled",
        alias = "canceled_order",
        alias = "cancelled_order"
    )]
    Cancelled,
    #[serde(rename = "returned")]
    Returned,
    #[serde(rename = "enroute_to_return", alias = "arrived_at_return")]
    Returning,
    #[serde(rename = "created", alias = "quote")]
    Created,
    #[serde(
        rename = "confirmed",
        alias = "enroute_to_pickup",
        alias = "arrived_at_pickup"
    )]
    BeforePickup,
    #[serde(
        rename = "picked_up",
        alias = "enroute_to_dropoff",
        alias = "arrived_at_dropoff"
    )]
    OutForDelivery,
}

impl OrderStatus {
    fn normalized(self) -> Status {
        match self {
            Self::Delivered => Status::Delivered,
            Self::Cancelled => Status::Cancelled,
            Self::Returned => Status::ReturnToSender,
            // A return still travelling must remain eligible for refresh.
            Self::Returning => Status::Failure,
            Self::Created | Self::BeforePickup => Status::PreTransit,
            Self::OutForDelivery => Status::OutForDelivery,
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct EtaDetails {
    #[serde(default, deserialize_with = "provider_optional")]
    actual_delivery_time: Option<String>,
    #[serde(default, deserialize_with = "provider_optional")]
    estimated_delivery_time: Option<String>,
}

#[derive(Deserialize)]
struct ProgressBar {
    steps: Vec<ProgressStep>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProgressStep {
    is_current: bool,
    step_type: String,
}

fn clean_datetime(value: Option<String>) -> Option<String> {
    let value = value.filter(|value| value.len() <= 64)?;
    time::OffsetDateTime::parse(&value, &time::format_description::well_known::Rfc3339)
        .ok()?
        .to_offset(time::UtcOffset::UTC)
        .format(&time::format_description::well_known::Rfc3339)
        .ok()
}

fn parse_page(body: &[u8], code: &str, tracking_number: &str) -> Result<Tracker, Error> {
    let unavailable = || provider_error(ErrorKind::RecipientStatusUnavailable);
    let html = std::str::from_utf8(body).map_err(|_| unavailable())?;
    let records = flight_records(html)?;
    let mut candidates = Vec::new();
    let mut remaining_nodes = 200_000;
    for value in records.values() {
        find_tracking_payloads(value, 0, &mut remaining_nodes, &mut candidates)?;
    }
    let [candidate] = candidates.as_slice() else {
        return Err(unavailable());
    };
    let resolved = resolve_references(candidate, &records, 0, &mut 50_000)?;
    let observation: RecipientTracking =
        serde_json::from_value(resolved).map_err(|_| unavailable())?;
    if observation.external_delivery_id != tracking_number {
        return Err(provider_error(ErrorKind::Rejected(
            Rejection::RecipientLinkMismatch,
        )));
    }
    let [order] = observation
        .get_order_status_response
        .merchant_order_status
        .as_slice()
    else {
        return Err(unavailable());
    };
    let status = order.order_details.order_status.normalized();
    if let Some(progress) = &order.progress_bar {
        let current = progress
            .steps
            .iter()
            .filter(|step| step.is_current)
            .collect::<Vec<_>>();
        if current.len() > 1
            || current.first().is_some_and(|step| {
                step.step_type == "STEP_TYPE_ORDER_COMPLETE" && !status.is_terminal()
            })
        {
            return Err(unavailable());
        }
    }
    let delivered_at = order
        .eta_details
        .as_ref()
        .and_then(|eta| clean_datetime(eta.actual_delivery_time.clone()));
    if delivered_at.is_some() && status != Status::Delivered {
        return Err(unavailable());
    }
    Ok(Tracker {
        provider: ProviderId::DoorDash,
        id: code.to_owned(),
        tracking_code: tracking_number.to_owned(),
        carrier: Carrier::DoorDash,
        status,
        public_url: Some(recipient_url(code)),
        carrier_tracking: CarrierTracking {
            estimated_delivery_at: order
                .eta_details
                .as_ref()
                .and_then(|eta| clean_datetime(eta.estimated_delivery_time.clone())),
            delivered_at,
            ..CarrierTracking::default()
        },
    })
}

impl ShippingApi for DoorDashClient {
    fn id(&self) -> ProviderId {
        ProviderId::DoorDash
    }

    fn blocked(&self) -> bool {
        self.blocked()
    }

    fn for_request(&self, request_id: &str, shipment_id: Option<&str>) -> Arc<dyn ShippingApi> {
        let mut client = self.clone();
        client.request_id = Some(request_id.into());
        if let Some(id) = shipment_id {
            client.log = client.log.new(slog::o!("shipment_id" => id.to_owned()));
        }
        Arc::new(client)
    }

    fn check_connection(&self) -> ApiFuture<'_, ()> {
        // There is no account or global connection to check. Admission and
        // refresh verify access to each original recipient link independently.
        Box::pin(async { Ok(()) })
    }

    fn reconcile_registration<'a>(
        &'a self,
        registration: &'a Registration,
    ) -> ApiFuture<'a, Option<Tracker>> {
        Box::pin(self.observe("reconcile", async move {
            let code = registration_code(registration)?;
            self.fetch(&code, &registration.tracking_number)
                .await
                .map(Some)
        }))
    }

    fn prepare_registration(
        &self,
        registration: Registration,
    ) -> ApiFuture<'_, Box<dyn PreparedRegistration>> {
        Box::pin(async move {
            let code = registration_code(&registration)?;
            let slot = self.request_slot().await?;
            Ok(Box::new(RecipientRegistration {
                client: self.clone(),
                slot,
                code,
                tracking_number: registration.tracking_number,
            }) as Box<dyn PreparedRegistration>)
        })
    }

    fn retrieve<'a>(
        &'a self,
        tracker_id: &'a str,
        tracking_number: &'a str,
    ) -> ApiFuture<'a, Tracker> {
        Box::pin(self.observe("retrieve", self.fetch(tracker_id, tracking_number)))
    }
}

struct RecipientRegistration {
    client: DoorDashClient,
    slot: OwnedMutexGuard<RequestSchedule>,
    code: String,
    tracking_number: String,
}

impl PreparedRegistration for RecipientRegistration {
    fn register(self: Box<Self>) -> ApiFuture<'static, Tracker> {
        Box::pin(async move {
            self.client
                .observe(
                    "register",
                    self.client
                        .fetch_in_slot(&self.code, &self.tracking_number, self.slot),
                )
                .await
        })
    }
}

#[cfg(test)]
mod tests {
    use std::{
        io::{Read, Write},
        net::TcpListener,
        sync::{
            atomic::{AtomicBool, Ordering},
            mpsc,
        },
        thread,
    };

    use serde::Serialize;

    use super::*;

    const NUMBER: &str = "DDTEST00000000001";
    const CODE: &str = "synthetic-recipient-code";

    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    struct FixturePayload<'a> {
        external_delivery_id: &'a str,
        get_order_status_response: FixtureResponse<'a>,
    }

    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    struct FixtureResponse<'a> {
        merchant_order_status: Vec<FixtureOrder<'a>>,
    }

    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    struct FixtureOrder<'a> {
        order_details: FixtureDetails<'a>,
        eta_details: FixtureEta<'a>,
        progress_bar: FixtureProgress,
    }

    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    struct FixtureDetails<'a> {
        order_status: &'a str,
    }

    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    struct FixtureEta<'a> {
        actual_delivery_time: Option<&'a str>,
        estimated_delivery_time: Option<&'a str>,
    }

    #[derive(Serialize)]
    struct FixtureProgress {
        steps: Vec<FixtureStep>,
    }

    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    struct FixtureStep {
        is_current: bool,
        step_type: &'static str,
    }

    fn payload(status: &str, number: &str, actual: Option<&str>) -> String {
        serde_json::to_string(&FixturePayload {
            external_delivery_id: number,
            get_order_status_response: FixtureResponse {
                merchant_order_status: vec![FixtureOrder {
                    order_details: FixtureDetails {
                        order_status: status,
                    },
                    eta_details: FixtureEta {
                        actual_delivery_time: actual,
                        estimated_delivery_time: Some("2026-10-15T15:00:00-07:00"),
                    },
                    progress_bar: FixtureProgress {
                        steps: vec![FixtureStep {
                            is_current: true,
                            step_type: if status == "delivered_order" {
                                "STEP_TYPE_ORDER_COMPLETE"
                            } else {
                                "STEP_TYPE_DASHER"
                            },
                        }],
                    },
                }],
            },
        })
        .unwrap()
    }

    fn page_from_flight(flight: &str) -> String {
        let midpoint = flight.len() / 2;
        let first = serde_json::to_string(&(1, &flight[..midpoint])).unwrap();
        let second = serde_json::to_string(&(1, &flight[midpoint..])).unwrap();
        format!(
            "<!doctype html><html><script>self.__next_f.push([2,null])</script>\
             <script>self.__next_f.push({first})</script>\
             <script>self.__next_f.push({second})</script></html>"
        )
    }

    fn page(status: &str, number: &str, actual: Option<&str>) -> String {
        // The page transport, rather than a separate JSON endpoint, is the
        // provider contract. Keep this synthetic tree shaped like its RSC root.
        let data = payload(status, number, actual);
        page_from_flight(&format!(
            "1:I[\"client-component\"]\n0:[\"$\",\"$L1\",null,{{\"platformProps\":{{\"initialReactQueryState\":{{\"queries\":[{{\"state\":{{\"data\":{data}}}}}]}}}}}}]\n"
        ))
    }

    fn registration() -> Registration {
        Registration {
            tracking_number: NUMBER.into(),
            carrier: Some(Carrier::DoorDash),
            recipient_url: Some(recipient_url(CODE)),
            started_at: 1,
        }
    }

    #[test]
    fn recipient_link_validation_never_accepts_an_arbitrary_destination() {
        assert_eq!(
            normalize_recipient_url(&format!(
                "https://WWW.DOORDASH.COM:443/orders/drive?urlCode={CODE}"
            ))
            .unwrap(),
            recipient_url(CODE)
        );
        for input in [
            "http://www.doordash.com/orders/drive?urlCode=code",
            "https://doordash.com/orders/drive?urlCode=code",
            "https://www.doordash.com.evil.invalid/orders/drive?urlCode=code",
            "https://www.doordash.com./orders/drive?urlCode=code",
            "https://www.doordash.com@evil.invalid/orders/drive?urlCode=code",
            "https://evil@www.doordash.com/orders/drive?urlCode=code",
            "https://@www.doordash.com/orders/drive?urlCode=code",
            "https://www.doordash.com:444/orders/drive?urlCode=code",
            "https://127.0.0.1/orders/drive?urlCode=code",
            "https://[::1]/orders/drive?urlCode=code",
            "https://track.doordash.com/order/code/track",
            "https://www.doordash.com/orders/drive/?urlCode=code",
            "https://www.doordash.com/orders/drive?trackingNumber=code",
            "https://www.doordash.com/orders/drive?urlCode=code&urlCode=second",
            "https://www.doordash.com/orders/drive?urlCode=code&next=https://evil.invalid",
            "https://www.doordash.com/orders/drive?urlCode=",
            "https://www.doordash.com/orders/drive?urlCode=code#fragment",
            "https://www.doordash.com/orders/drive?urlCode=co%0Ade",
            "https://www.doordash.com/orders/drive?urlCode=co%2Fde",
            "https://www.doordash.com/orders/drive?urlCode=co+de",
            "https://www.doordash.com/orders/drive?urlCode=code ",
            "https://www.doordash.com/orders/drive?urlCode=code\n",
            "https://www.doordash.com\\orders\\drive?urlCode=code",
        ] {
            let error = normalize_recipient_url(input).unwrap_err();
            assert_eq!(error.error_code(), "tracking_recipient_url_invalid");
        }
        let error = normalize_recipient_url(&format!(
            "https://www.doordash.com/orders/drive?urlCode={CODE}&extra=private"
        ))
        .unwrap_err();
        assert!(!format!("{error:?} {error}").contains(CODE));
        assert!(normalize_recipient_url(&recipient_url(&"a".repeat(MAX_CODE_BYTES))).is_ok());
        assert!(normalize_recipient_url(&recipient_url(&"a".repeat(MAX_CODE_BYTES + 1))).is_err());
    }

    #[test]
    fn delivered_page_keeps_actual_time_without_inventing_events() {
        let tracker = parse_page(
            page("delivered_order", NUMBER, Some("2026-10-15T12:34:56-07:00")).as_bytes(),
            CODE,
            NUMBER,
        )
        .unwrap();
        assert_eq!(tracker.carrier, Carrier::DoorDash);
        assert_eq!(tracker.id, CODE);
        assert_eq!(tracker.tracking_code, NUMBER);
        assert_eq!(tracker.status, Status::Delivered);
        assert_eq!(
            tracker.public_url.as_deref(),
            Some(recipient_url(CODE).as_str())
        );
        assert_eq!(
            tracker.carrier_tracking.delivered_at.as_deref(),
            Some("2026-10-15T19:34:56Z")
        );
        assert!(tracker.carrier_tracking.events.is_empty());
    }

    #[test]
    fn estimated_time_and_complete_badge_are_not_a_delivery_scan() {
        let tracker = parse_page(page("created", NUMBER, None).as_bytes(), CODE, NUMBER).unwrap();
        assert_eq!(tracker.status, Status::PreTransit);
        assert!(tracker.carrier_tracking.delivered_at.is_none());
        assert_eq!(
            tracker.carrier_tracking.estimated_delivery_at.as_deref(),
            Some("2026-10-15T22:00:00Z")
        );
        let contradictory =
            page("created", NUMBER, None).replace("STEP_TYPE_DASHER", "STEP_TYPE_ORDER_COMPLETE");
        assert!(parse_page(contradictory.as_bytes(), CODE, NUMBER).is_err());
        let tracker = parse_page(
            page("delivered_order", NUMBER, None).as_bytes(),
            CODE,
            NUMBER,
        )
        .unwrap();
        assert_eq!(tracker.status, Status::Delivered);
        assert!(tracker.carrier_tracking.delivered_at.is_none());
    }

    #[test]
    fn missing_unknown_mismatched_and_ambiguous_pages_fail_closed() {
        for html in [
            "<html>Please sign in</html>".to_owned(),
            "<html>Verify you are human</html>".to_owned(),
            page("brand_new_state", NUMBER, None),
            page("created", NUMBER, Some("2026-10-15T19:34:56Z")),
            page_from_flight(&format!(
                "1:{}\n2:{}\n",
                payload("created", NUMBER, None),
                payload("created", NUMBER, None)
            )),
        ] {
            let error = parse_page(html.as_bytes(), CODE, NUMBER).unwrap_err();
            assert_eq!(error.error_code(), "recipient_page_unavailable");
            assert!(!error.rejects_registration());
        }
        let mismatch = parse_page(
            page("created", "DDOTHER0000000001", None).as_bytes(),
            CODE,
            NUMBER,
        )
        .unwrap_err();
        assert_eq!(mismatch.error_code(), "tracking_recipient_url_mismatch");
        assert!(mismatch.rejects_registration());
    }

    #[test]
    fn flight_reference_cycles_and_text_record_injection_are_rejected() {
        let data = payload("created", NUMBER, None);
        let referenced = data.replace("{\"orderStatus\":\"created\"}", "\"$a\"");
        let page = page_from_flight(&format!(
            "0:{referenced}\na:{{\"orderStatus\":\"created\"}}\n"
        ));
        assert_eq!(
            parse_page(page.as_bytes(), CODE, NUMBER).unwrap().status,
            Status::PreTransit
        );
        let cyclic = page_from_flight(&format!("0:{referenced}\na:\"$a\"\n"));
        assert!(parse_page(cyclic.as_bytes(), CODE, NUMBER).is_err());
        let injected = format!("untrusted text\n1:{data}\n");
        let text_only = page_from_flight(&format!("0:T{:x},{injected}\n", injected.len()));
        assert!(parse_page(text_only.as_bytes(), CODE, NUMBER).is_err());
    }

    struct Reply {
        status: &'static str,
        body: Vec<u8>,
        headers: Vec<(&'static str, String)>,
        delay: Duration,
        declared_length: Option<usize>,
        omit_content_length: bool,
    }

    impl Reply {
        fn html(body: String) -> Self {
            Self {
                status: "200 OK",
                body: body.into_bytes(),
                headers: Vec::new(),
                delay: Duration::ZERO,
                declared_length: None,
                omit_content_length: false,
            }
        }
    }

    struct Fixture {
        url: String,
        requests: mpsc::Receiver<String>,
        stop: Arc<AtomicBool>,
        thread: Option<thread::JoinHandle<()>>,
    }

    impl Fixture {
        fn start(replies: Vec<Reply>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let url = format!("http://{}/", listener.local_addr().unwrap());
            listener.set_nonblocking(true).unwrap();
            let (send, requests) = mpsc::channel();
            let stop = Arc::new(AtomicBool::new(false));
            let ending = stop.clone();
            let thread = thread::spawn(move || {
                for reply in replies {
                    let mut stream = loop {
                        match listener.accept() {
                            Ok((stream, _)) => break stream,
                            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                                if ending.load(Ordering::Relaxed) {
                                    return;
                                }
                                thread::sleep(Duration::from_millis(5));
                            }
                            Err(_) => return,
                        }
                    };
                    stream.set_nonblocking(false).unwrap();
                    stream
                        .set_read_timeout(Some(Duration::from_secs(1)))
                        .unwrap();
                    stream
                        .set_write_timeout(Some(Duration::from_secs(1)))
                        .unwrap();
                    let mut request = Vec::new();
                    let mut buffer = [0; 1024];
                    while !request.windows(4).any(|part| part == b"\r\n\r\n") {
                        let Ok(length) = stream.read(&mut buffer) else {
                            return;
                        };
                        if length == 0 {
                            return;
                        }
                        request.extend_from_slice(&buffer[..length]);
                    }
                    let _ = send.send(String::from_utf8(request).unwrap());
                    thread::sleep(reply.delay);
                    let mut header = format!(
                        "HTTP/1.1 {}\r\nContent-Type: text/html; charset=utf-8\r\nConnection: close\r\n",
                        reply.status
                    );
                    if !reply.omit_content_length {
                        header.push_str(&format!(
                            "Content-Length: {}\r\n",
                            reply.declared_length.unwrap_or(reply.body.len())
                        ));
                    }
                    for (name, value) in reply.headers {
                        header.push_str(&format!("{name}: {value}\r\n"));
                    }
                    header.push_str("\r\n");
                    if stream.write_all(header.as_bytes()).is_ok() {
                        let _ = stream.write_all(&reply.body);
                    }
                }
            });
            Self {
                url,
                requests,
                stop,
                thread: Some(thread),
            }
        }

        fn client(&self) -> DoorDashClient {
            DoorDashClient::for_loopback(&self.url).unwrap()
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            self.thread.take().unwrap().join().unwrap();
        }
    }

    #[tokio::test]
    async fn common_provider_contract_fetches_only_get_and_reuses_original_code() {
        let fixture = Fixture::start(vec![Reply::html(page("created", NUMBER, None))]);
        let client = fixture.client();
        let tracker = ShippingApi::reconcile_registration(&client, &registration())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(tracker.status, Status::PreTransit);
        let request = fixture.requests.try_recv().unwrap();
        assert!(request.starts_with(&format!("GET {RECIPIENT_PATH}?urlCode={CODE} HTTP/1.1\r\n")));
        assert!(!request.to_ascii_lowercase().contains("authorization:"));
        assert!(!request.to_ascii_lowercase().contains("cookie:"));

        let fixture = Fixture::start(vec![Reply::html(page("delivered_order", NUMBER, None))]);
        let client = fixture.client();
        let reserved = ShippingApi::prepare_registration(&client, registration())
            .await
            .unwrap();
        let tracker = reserved.register().await.unwrap();
        assert_eq!(tracker.id, CODE);
        assert!(fixture.requests.try_recv().unwrap().starts_with("GET "));
    }

    #[tokio::test]
    async fn missing_or_wrong_recipient_input_never_reaches_network() {
        let fixture = Fixture::start(vec![]);
        let client = fixture.client();
        let mut input = registration();
        input.recipient_url = None;
        let error = ShippingApi::reconcile_registration(&client, &input)
            .await
            .unwrap_err();
        assert_eq!(error.error_code(), "tracking_recipient_url_required");
        input.recipient_url = Some(recipient_url(CODE));
        input.carrier = Some(Carrier::Usps);
        let error = ShippingApi::reconcile_registration(&client, &input)
            .await
            .unwrap_err();
        assert_eq!(error.error_code(), "tracking_carrier_mismatch");
        assert!(fixture.requests.try_recv().is_err());
        for base in [
            "http://example.invalid/",
            "http://localhost/",
            "http://127.0.0.1/private/",
            "http://user@127.0.0.1/",
        ] {
            assert!(DoorDashClient::for_loopback(base).is_err());
        }
    }

    #[tokio::test]
    async fn dropping_a_reservation_releases_shared_capacity_without_a_request() {
        let fixture = Fixture::start(vec![Reply::html(page("created", NUMBER, None))]);
        let client = fixture.client();
        let reserved = ShippingApi::prepare_registration(&client, registration())
            .await
            .unwrap();
        assert!(client.blocked());
        assert!(fixture.requests.try_recv().is_err());
        drop(reserved);
        assert!(!client.blocked());
        assert_eq!(
            ShippingApi::retrieve(&client, CODE, NUMBER)
                .await
                .unwrap()
                .status,
            Status::PreTransit
        );
        assert_eq!(fixture.requests.try_iter().count(), 1);
    }

    #[tokio::test]
    async fn redirect_is_not_followed_and_provider_error_body_is_redacted() {
        let fixture = Fixture::start(vec![Reply {
            status: "302 Found",
            headers: vec![("Location", "http://169.254.169.254/private".into())],
            ..Reply::html(format!("private response with {CODE} and {NUMBER}"))
        }]);
        let error = ShippingApi::retrieve(&fixture.client(), CODE, NUMBER)
            .await
            .unwrap_err();
        assert_eq!(error.provider_status(), Some(302));
        let formatted = format!("{error:?} {error}");
        assert!(!formatted.contains(CODE));
        assert!(!formatted.contains(NUMBER));
        assert!(!formatted.contains("169.254"));
        assert_eq!(fixture.requests.try_iter().count(), 1);
    }

    #[tokio::test]
    async fn rate_limits_and_retry_after_apply_to_all_request_clones() {
        let fixture = Fixture::start(vec![Reply {
            status: "429 Too Many Requests",
            headers: vec![("Retry-After", "120".into())],
            ..Reply::html("private error".into())
        }]);
        let client = fixture.client();
        let scoped = client.for_request(&Uuid::now_v7().to_string(), None);
        let error = scoped.retrieve(CODE, NUMBER).await.unwrap_err();
        assert_eq!(error.retry_after(), Some(120));
        assert!(client.blocked());
        let error = ShippingApi::retrieve(&client, CODE, NUMBER)
            .await
            .unwrap_err();
        assert!(matches!(error.kind, ErrorKind::Throttled { .. }));
        assert_eq!(fixture.requests.try_iter().count(), 1);
    }

    #[tokio::test]
    async fn slow_or_oversized_responses_have_bounded_safe_errors() {
        let fixture = Fixture::start(vec![Reply {
            declared_length: Some(MAX_BODY_BYTES + 1),
            ..Reply::html("body withheld".into())
        }]);
        let error = ShippingApi::retrieve(&fixture.client(), CODE, NUMBER)
            .await
            .unwrap_err();
        assert!(matches!(error.kind, ErrorKind::ResponseTooLarge));

        let fixture = Fixture::start(vec![Reply {
            body: vec![b'x'; MAX_BODY_BYTES + 1],
            omit_content_length: true,
            ..Reply::html(String::new())
        }]);
        let error = ShippingApi::retrieve(&fixture.client(), CODE, NUMBER)
            .await
            .unwrap_err();
        assert!(matches!(error.kind, ErrorKind::ResponseTooLarge));

        let fixture = Fixture::start(vec![Reply {
            delay: Duration::from_millis(100),
            ..Reply::html(page("created", NUMBER, None))
        }]);
        let mut client = fixture.client();
        client.client = Client::builder()
            .timeout(Duration::from_millis(20))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        let error = ShippingApi::retrieve(&client, CODE, NUMBER)
            .await
            .unwrap_err();
        assert!(matches!(error.kind, ErrorKind::Timeout));
        let formatted = format!("{error:?} {error}");
        assert!(!formatted.contains(CODE));
        assert!(!formatted.contains(NUMBER));
        assert!(!formatted.contains(&fixture.url));
    }
}
