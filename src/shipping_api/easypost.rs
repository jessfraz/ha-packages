//! EasyPost wire formats, authentication, pacing and registration recovery.

use reqwest::{Client, RequestBuilder, Response, Url};
use serde::{Deserialize, Serialize};
use std::{net::IpAddr, sync::Arc, time::Duration};
use tokio::sync::{Mutex, OwnedMutexGuard};
use tokio::time::Instant;
use uuid::Uuid;

use crate::{
    carrier::Carrier,
    models::{CarrierEvent, CarrierTracking, ProviderValue, Status, provider_optional},
    shipping_api::{
        PreparedRegistration, ShippingApi,
        error::{Error as SyncError, ErrorKind, Rejection},
        types::{ApiFuture, ProviderId, Registration, Tracker},
    },
};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
const MAX_BODY_BYTES: usize = 256 * 1024;
const MAX_RETRY_SECONDS: i64 = 6 * 60 * 60;

fn provider_error(kind: ErrorKind) -> SyncError {
    SyncError::new(ProviderId::EasyPost, kind)
}

#[derive(Clone, Copy, Debug, Deserialize)]
enum TrackerRejection {
    #[serde(rename = "TRACKER.NO_CARRIER_FOR_CODE")]
    NoCarrierForCode,
    #[serde(rename = "TRACKER.CARRIER_CODE_MISMATCH")]
    CarrierCodeMismatch,
    #[serde(rename = "TRACKER.MULTIPLE_CARRIERS_FOR_CODE")]
    MultipleCarriersForCode,
    #[serde(rename = "TRACKER.INVALID_PARAMS")]
    InvalidParams,
    #[serde(rename = "TRACKER.INVALID_TEST_CODE")]
    InvalidTestCode,
    #[serde(rename = "TRACKER.NO_CARRIER_ACCOUNT")]
    NoCarrierAccount,
    #[serde(rename = "CREDENTIALS_NOT_FOUND")]
    CarrierCredentialsMissing,
}

impl TrackerRejection {
    fn rejection(self) -> Rejection {
        match self {
            Self::NoCarrierForCode => Rejection::NoCarrierForCode,
            Self::CarrierCodeMismatch => Rejection::CarrierCodeMismatch,
            Self::MultipleCarriersForCode => Rejection::MultipleCarriersForCode,
            Self::InvalidParams => Rejection::InvalidParams,
            Self::InvalidTestCode => Rejection::InvalidTestCode,
            Self::NoCarrierAccount => Rejection::NoCarrierAccount,
            Self::CarrierCredentialsMissing => Rejection::CarrierCredentialsMissing,
        }
    }
    fn code(self) -> &'static str {
        match self {
            Self::NoCarrierForCode => "TRACKER.NO_CARRIER_FOR_CODE",
            Self::CarrierCodeMismatch => "TRACKER.CARRIER_CODE_MISMATCH",
            Self::MultipleCarriersForCode => "TRACKER.MULTIPLE_CARRIERS_FOR_CODE",
            Self::InvalidParams => "TRACKER.INVALID_PARAMS",
            Self::InvalidTestCode => "TRACKER.INVALID_TEST_CODE",
            Self::NoCarrierAccount => "TRACKER.NO_CARRIER_ACCOUNT",
            Self::CarrierCredentialsMissing => "CREDENTIALS_NOT_FOUND",
        }
    }
}
#[derive(Deserialize)]
struct TrackerError {
    error: TrackerErrorCode,
}
#[derive(Deserialize)]
struct TrackerErrorCode {
    code: TrackerRejection,
}

#[derive(Clone)]
pub struct EasyPostClient {
    client: Client,
    api_key: String,
    base: Url,
    requests: Arc<Mutex<RequestSchedule>>,
    log: slog::Logger,
    request_id: Option<String>,
}

struct RequestSchedule {
    next_start: Instant,
    cooldown: Option<Instant>,
    rate_limits: u32,
}

#[derive(Deserialize)]
struct ProviderTracker {
    id: String,
    tracking_code: String,
    carrier: Carrier,
    status: Status,
    #[serde(default)]
    public_url: Option<String>,
    #[serde(default, deserialize_with = "provider_optional")]
    tracking_details: Option<Vec<ProviderValue<CarrierEvent>>>,
    #[serde(default, deserialize_with = "provider_optional")]
    signed_by: Option<String>,
    #[serde(default, deserialize_with = "provider_optional")]
    est_delivery_date: Option<String>,
}

impl From<ProviderTracker> for Tracker {
    fn from(raw: ProviderTracker) -> Self {
        let mut carrier_tracking = CarrierTracking {
            events: raw
                .tracking_details
                .unwrap_or_default()
                .into_iter()
                .filter_map(|event| match event {
                    ProviderValue::Value(event) => normalize_event(event),
                    ProviderValue::Ignored(_) => None,
                })
                .collect(),
            signed_by: clean_text(raw.signed_by, 200),
            estimated_delivery_at: clean_datetime(raw.est_delivery_date),
            delivered_at: None,
            events_truncated: false,
        };
        let incoming = std::mem::take(&mut carrier_tracking);
        carrier_tracking.merge(&incoming);
        Self {
            id: raw.id,
            tracking_code: raw.tracking_code,
            carrier: raw.carrier,
            status: raw.status,
            public_url: raw.public_url,
            carrier_tracking,
        }
    }
}

fn clean_text(value: Option<String>, maximum: usize) -> Option<String> {
    let value = value?;
    let mut cleaned = String::new();
    for character in value
        .trim()
        .chars()
        .filter(|character| !character.is_control() || matches!(character, '\n' | '\t'))
    {
        if cleaned.len() + character.len_utf8() > maximum {
            break;
        }
        cleaned.push(character);
    }
    (!cleaned.is_empty()).then_some(cleaned)
}

fn clean_datetime(value: Option<String>) -> Option<String> {
    let value = value?;
    if value.len() > 64 {
        return None;
    }
    time::OffsetDateTime::parse(&value, &time::format_description::well_known::Rfc3339)
        .ok()?
        .to_offset(time::UtcOffset::UTC)
        .format(&time::format_description::well_known::Rfc3339)
        .ok()
}

fn normalize_event(mut event: CarrierEvent) -> Option<CarrierEvent> {
    event.status = event.status.filter(|status| *status != Status::Expected);
    event.status_detail = clean_text(event.status_detail, 100);
    event.message = clean_text(event.message, 1024);
    event.description = clean_text(event.description, 2048);
    event.datetime = clean_datetime(event.datetime);
    event.source = clean_text(event.source, 100);
    event.carrier_code = clean_text(event.carrier_code, 100);
    event.tracking_location = event.tracking_location.and_then(|mut location| {
        location.city = clean_text(location.city, 200);
        location.state = clean_text(location.state, 100);
        location.country = clean_text(location.country, 100);
        location.zip = clean_text(location.zip, 40);
        (location != crate::models::TrackingLocation::default()).then_some(location)
    });
    (event != CarrierEvent::default()).then_some(event)
}

#[derive(Deserialize)]
struct TrackerPage {
    trackers: Vec<TrackerObject>,
    #[serde(rename = "has_more")]
    _has_more: bool,
}

#[derive(Deserialize)]
#[serde(tag = "object")]
enum TrackerObject {
    Tracker,
}

#[derive(Serialize)]
struct RegisterTracker<'a> {
    tracker: RegisterTracking<'a>,
}

#[derive(Serialize)]
struct RegisterTracking<'a> {
    tracking_code: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    carrier: Option<Carrier>,
}

impl EasyPostClient {
    pub fn new(api_key: impl Into<String>) -> Result<Self, SyncError> {
        Self::build(api_key.into(), "https://api.easypost.com/v2/")
    }

    /// Use a real loopback HTTP server for integration tests without exposing a
    /// configurable production destination for the provider credential.
    pub fn for_loopback(api_key: impl Into<String>, base_url: &str) -> Result<Self, SyncError> {
        let url = Url::parse(base_url).map_err(|_| provider_error(ErrorKind::Configuration))?;
        let loopback = url
            .host_str()
            .and_then(|host| host.trim_matches(['[', ']']).parse::<IpAddr>().ok())
            .is_some_and(|address| address.is_loopback());
        if !loopback || !matches!(url.scheme(), "http" | "https") {
            return Err(provider_error(ErrorKind::Configuration));
        }
        Self::build(api_key.into(), base_url)
    }

    fn build(api_key: String, base_url: &str) -> Result<Self, SyncError> {
        let base = Url::parse(base_url).map_err(|_| provider_error(ErrorKind::Configuration))?;
        if api_key.trim().is_empty()
            || !base.username().is_empty()
            || base.password().is_some()
            || base.query().is_some()
            || base.fragment().is_some()
            || !base.path().ends_with('/')
        {
            return Err(provider_error(ErrorKind::Configuration));
        }
        let client = Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(concat!("shipments/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|_| provider_error(ErrorKind::Configuration))?;
        Ok(Self {
            client,
            api_key,
            base,
            requests: Arc::new(Mutex::new(RequestSchedule {
                next_start: Instant::now(),
                cooldown: None,
                rate_limits: 0,
            })),
            log: crate::logging::logger(),
            request_id: None,
        })
    }

    pub fn with_logger(mut self, log: slog::Logger) -> Self {
        self.log = log;
        self
    }

    pub fn with_request_id(&self, request_id: &str) -> Self {
        let mut client = self.clone();
        client.request_id = Some(request_id.into());
        client
    }

    async fn observe<T>(
        &self,
        operation: &'static str,
        future: impl std::future::Future<Output = Result<T, SyncError>>,
    ) -> Result<T, SyncError> {
        let request_id = self
            .request_id
            .clone()
            .unwrap_or_else(|| Uuid::now_v7().to_string());
        let started = Instant::now();
        let result = future.await;
        let error = result.as_ref().err();
        let outcome = match error.map(|error| &error.kind) {
            None => "success",
            Some(ErrorKind::Throttled { .. }) => "throttled",
            Some(
                ErrorKind::Http { status: 429, .. }
                | ErrorKind::ProviderRejected { status: 429, .. },
            ) => "rate_limited",
            Some(ErrorKind::ProviderRejected { status, .. })
                if !matches!(status, 400 | 404 | 422) =>
            {
                "error"
            }
            Some(
                ErrorKind::Rejected(_)
                | ErrorKind::ProviderRejected { .. }
                | ErrorKind::RegistrationAmbiguous
                | ErrorKind::RegistrationExpired,
            ) => "rejected",
            Some(_) => "error",
        };
        slog::info!(self.log, "provider_request";
            "request_id" => request_id, "operation" => operation, "outcome" => outcome,
            "error_code" => error.map(SyncError::error_code), "provider_status" => error.and_then(SyncError::provider_status),
            "provider_error_code" => error.and_then(SyncError::provider_error_code),
            "provider_request_id" => error.and_then(|error| error.provider_request_id).map(|id| id.to_string()),
            "retry_after_seconds" => error.and_then(SyncError::retry_after), "provider" => ProviderId::EasyPost.as_str(), "duration_ms" => started.elapsed().as_millis());
        result
    }

    // All clones share one outbound slot, including API verification and health
    // checks. Reject contention instead of holding an unbounded request queue or
    // waiting out a provider cooldown inside a shipment's lease.
    async fn send(&self, request: RequestBuilder) -> Result<Vec<u8>, SyncError> {
        let schedule = self.request_slot().await?;
        self.send_in_slot(request, schedule).await
    }

    async fn request_slot(&self) -> Result<OwnedMutexGuard<RequestSchedule>, SyncError> {
        let schedule = self
            .requests
            .clone()
            .try_lock_owned()
            .map_err(|_| provider_error(ErrorKind::Throttled { retry_after: 1 }))?;
        if let Some(until) = schedule.cooldown.filter(|until| *until > Instant::now()) {
            let remaining = until.saturating_duration_since(Instant::now());
            return Err(provider_error(ErrorKind::Throttled {
                retry_after: i64::try_from(remaining.as_secs()).unwrap_or(MAX_RETRY_SECONDS) + 1,
            }));
        }
        tokio::time::sleep_until(schedule.next_start).await;
        Ok(schedule)
    }

    async fn send_in_slot(
        &self,
        request: RequestBuilder,
        mut schedule: OwnedMutexGuard<RequestSchedule>,
    ) -> Result<Vec<u8>, SyncError> {
        schedule.next_start = Instant::now() + Duration::from_secs(1);
        let result = match request.send().await {
            Ok(response) => {
                let request_id = response
                    .headers()
                    .get("x-ep-request-uuid")
                    .and_then(|value| value.to_str().ok())
                    .and_then(|value| Uuid::parse_str(value).ok());
                Self::read_body(response).await.map_err(|mut error| {
                    error.provider_request_id = request_id;
                    error
                })
            }
            Err(error) => Err(redact_request_error(error)),
        };
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
            Ok(body) => {
                schedule.rate_limits = 0;
                schedule.cooldown = None;
                Ok(body)
            }
            Err(error) => Err(error),
        }
    }

    fn blocked(&self) -> bool {
        self.requests.try_lock().map_or(true, |schedule| {
            schedule
                .cooldown
                .is_some_and(|until| until > Instant::now())
        })
    }

    /// Check authenticated access with one read-only list request. No trackers
    /// are created, and no returned tracking or account details leave this call.
    pub async fn check_connection(&self) -> Result<(), SyncError> {
        self.observe("health", async {
            let url = self
                .base
                .join("trackers")
                .map_err(|_| provider_error(ErrorKind::Configuration))?;
            let request = self
                .client
                .get(url)
                .query(&[("page_size", 1_u8)])
                .basic_auth(&self.api_key, Some(""));
            let page: TrackerPage = serde_json::from_slice(&self.send(request).await?)
                .map_err(|_| provider_error(ErrorKind::InvalidConnectionResponse))?;
            if page.trackers.len() > 1 {
                return Err(provider_error(ErrorKind::InvalidConnectionResponse));
            }
            Ok(())
        })
        .await
    }

    pub async fn register(
        &self,
        tracking_code: &str,
        carrier: Option<Carrier>,
    ) -> Result<Tracker, SyncError> {
        self.observe("register", async {
            let slot = self.request_slot().await?;
            self.register_in_slot(tracking_code, carrier, slot).await
        })
        .await
    }

    async fn register_in_slot(
        &self,
        tracking_code: &str,
        carrier: Option<Carrier>,
        slot: OwnedMutexGuard<RequestSchedule>,
    ) -> Result<Tracker, SyncError> {
        let url = self
            .base
            .join("trackers")
            .map_err(|_| provider_error(ErrorKind::Configuration))?;
        let request = self
            .client
            .post(url)
            .basic_auth(&self.api_key, Some(""))
            .json(&RegisterTracker {
                tracker: RegisterTracking {
                    tracking_code,
                    carrier,
                },
            });
        let tracker = Self::read_tracker(&self.send_in_slot(request, slot).await?, tracking_code)?;
        if carrier.is_some_and(|carrier| carrier != tracker.carrier) {
            return Err(provider_error(ErrorKind::Rejected(
                Rejection::CarrierCodeMismatch,
            )));
        }
        Ok(tracker)
    }

    /// Resolve an earlier POST whose response may have been lost. This lookup
    /// never creates trackers; callers must not POST when its coverage is uncertain.
    pub async fn reconcile_registration(
        &self,
        tracking_code: &str,
        carrier: Option<Carrier>,
        first_attempt: i64,
    ) -> Result<Option<Tracker>, SyncError> {
        self.observe("reconcile", async {
            let now = time::OffsetDateTime::now_utc().unix_timestamp();
            let start = shift_calendar_months(first_attempt, -3)?;
            let start = time::OffsetDateTime::from_unix_timestamp(start)
                .map_err(|_| provider_error(ErrorKind::Configuration))?
                .format(&time::format_description::well_known::Rfc3339)
                .map_err(|_| provider_error(ErrorKind::Configuration))?;
            let end = time::OffsetDateTime::from_unix_timestamp(now.saturating_add(60))
                .map_err(|_| provider_error(ErrorKind::Configuration))?
                .format(&time::format_description::well_known::Rfc3339)
                .map_err(|_| provider_error(ErrorKind::Configuration))?;
            let mut query = vec![
                ("tracking_codes[]", tracking_code.to_owned()),
                ("page_size", "100".to_owned()),
                ("start_datetime", start),
                ("end_datetime", end),
            ];
            if let Some(carrier) = carrier {
                query.push(("carrier", carrier.to_string()));
            }
            let url = self
                .base
                .join("trackers")
                .map_err(|_| provider_error(ErrorKind::Configuration))?;
            let request = self
                .client
                .get(url)
                .query(&query)
                .basic_auth(&self.api_key, Some(""));
            #[derive(Deserialize)]
            struct Candidates {
                trackers: Vec<ProviderTracker>,
                has_more: bool,
            }
            let page: Candidates = serde_json::from_slice(&self.send(request).await?)
                .map_err(|_| provider_error(ErrorKind::InvalidResponse))?;
            if page.has_more || page.trackers.len() > 1 {
                return Err(provider_error(ErrorKind::RegistrationAmbiguous));
            }
            let tracker = page
                .trackers
                .into_iter()
                .next()
                .map(|tracker| {
                    let tracker = Self::validate_tracker(tracker.into(), tracking_code)?;
                    if carrier.is_some_and(|carrier| carrier != tracker.carrier) {
                        return Err(provider_error(ErrorKind::InvalidResponse));
                    }
                    Ok(tracker)
                })
                .transpose()?;
            if tracker.is_none() && now >= shift_calendar_months(first_attempt, 3)? {
                return Err(provider_error(ErrorKind::RegistrationExpired));
            }
            Ok(tracker)
        })
        .await
    }

    pub async fn retrieve(
        &self,
        tracker_id: &str,
        tracking_code: &str,
    ) -> Result<Tracker, SyncError> {
        self.observe("retrieve", async {
            if !valid_tracker_id(tracker_id) {
                return Err(provider_error(ErrorKind::InvalidResponse));
            }
            let mut url = self
                .base
                .join("trackers/")
                .map_err(|_| provider_error(ErrorKind::Configuration))?;
            url.path_segments_mut()
                .map_err(|_| provider_error(ErrorKind::Configuration))?
                .pop_if_empty()
                .push(tracker_id);
            let request = self.client.get(url).basic_auth(&self.api_key, Some(""));
            let tracker = Self::read_tracker(&self.send(request).await?, tracking_code)?;
            if tracker.id != tracker_id {
                return Err(provider_error(ErrorKind::InvalidResponse));
            }
            Ok(tracker)
        })
        .await
    }

    fn read_tracker(body: &[u8], tracking_code: &str) -> Result<Tracker, SyncError> {
        let tracker: ProviderTracker =
            serde_json::from_slice(body).map_err(|_| provider_error(ErrorKind::InvalidResponse))?;
        Self::validate_tracker(tracker.into(), tracking_code)
    }

    fn validate_tracker(mut tracker: Tracker, tracking_code: &str) -> Result<Tracker, SyncError> {
        if !valid_tracker_id(&tracker.id)
            || tracker.tracking_code != tracking_code
            || tracker.status == Status::Expected
        {
            return Err(provider_error(ErrorKind::InvalidResponse));
        }
        tracker.public_url = tracker.public_url.and_then(safe_tracking_url);
        Ok(tracker)
    }

    async fn read_body(mut response: Response) -> Result<Vec<u8>, SyncError> {
        let status = response.status();
        let http_error = if !status.is_success() {
            // Accept bounded delta-seconds. Ignore unparseable, zero, or
            // excessive advice; none of the raw header or body is persisted.
            let retry_after = response
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.parse::<i64>().ok())
                .filter(|seconds| (1..=MAX_RETRY_SECONDS).contains(seconds));
            let error = provider_error(ErrorKind::Http {
                status: status.as_u16(),
                retry_after,
            });
            Some(error)
        } else {
            None
        };
        if response
            .content_length()
            .is_some_and(|length| length > MAX_BODY_BYTES as u64)
        {
            return Err(http_error.unwrap_or(provider_error(ErrorKind::ResponseTooLarge)));
        }
        let mut body = Vec::new();
        loop {
            let chunk = match response.chunk().await {
                Ok(Some(chunk)) => chunk,
                Ok(None) => break,
                Err(error) => return Err(http_error.unwrap_or_else(|| redact_request_error(error))),
            };
            if chunk.len() > MAX_BODY_BYTES.saturating_sub(body.len()) {
                return Err(http_error.unwrap_or(provider_error(ErrorKind::ResponseTooLarge)));
            }
            body.extend_from_slice(&chunk);
        }
        if let Some(http_error) = http_error {
            let retry_after = http_error.retry_after();
            return Err(serde_json::from_slice::<TrackerError>(&body)
                .map(|parsed| {
                    provider_error(ErrorKind::ProviderRejected {
                        rejection: parsed.error.code.rejection(),
                        code: parsed.error.code.code(),
                        status: status.as_u16(),
                        retry_after,
                    })
                })
                .unwrap_or(http_error));
        }
        Ok(body)
    }
}

fn safe_tracking_url(value: String) -> Option<String> {
    if value.len() > 2048 {
        return None;
    }
    let url = Url::parse(&value).ok()?;
    let normalized = url.to_string();
    (url.scheme() == "https"
        && url.host_str() == Some("track.easypost.com")
        && url.username().is_empty()
        && url.password().is_none()
        && url.port().is_none()
        && normalized.len() <= 2048)
        .then_some(normalized)
}

fn valid_tracker_id(id: &str) -> bool {
    id.starts_with("trk_")
        && id.len() > 4
        && id.len() <= 128
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

fn redact_request_error(error: reqwest::Error) -> SyncError {
    if error.is_timeout() {
        provider_error(ErrorKind::Timeout)
    } else {
        provider_error(ErrorKind::Connection)
    }
}

// EasyPost documents a calendar-month deduplication window. Clamp month-end
// days explicitly rather than approximating three months with ninety days.
fn shift_calendar_months(timestamp: i64, months: i32) -> Result<i64, SyncError> {
    let at = time::OffsetDateTime::from_unix_timestamp(timestamp)
        .map_err(|_| provider_error(ErrorKind::Configuration))?;
    let ordinal = at
        .year()
        .checked_mul(12)
        .and_then(|year| year.checked_add(i32::from(u8::from(at.month())) - 1))
        .and_then(|month| month.checked_add(months))
        .ok_or(provider_error(ErrorKind::Configuration))?;
    let year = ordinal.div_euclid(12);
    let month = time::Month::try_from(
        u8::try_from(ordinal.rem_euclid(12) + 1)
            .map_err(|_| provider_error(ErrorKind::Configuration))?,
    )
    .map_err(|_| provider_error(ErrorKind::Configuration))?;
    for day in (1..=at.day()).rev() {
        if let Ok(date) = time::Date::from_calendar_date(year, month, day) {
            return Ok(date.with_time(at.time()).assume_utc().unix_timestamp());
        }
    }
    Err(provider_error(ErrorKind::Configuration))
}

impl ShippingApi for EasyPostClient {
    fn id(&self) -> ProviderId {
        ProviderId::EasyPost
    }
    fn blocked(&self) -> bool {
        self.blocked()
    }
    fn for_request(&self, request_id: &str, shipment_id: Option<&str>) -> Arc<dyn ShippingApi> {
        let mut client = self.with_request_id(request_id);
        if let Some(id) = shipment_id {
            client.log = client.log.new(slog::o!("shipment_id" => id.to_owned()));
        }
        Arc::new(client)
    }
    fn check_connection(&self) -> ApiFuture<'_, ()> {
        Box::pin(self.check_connection())
    }
    fn reconcile_registration<'a>(
        &'a self,
        registration: &'a Registration,
    ) -> ApiFuture<'a, Option<Tracker>> {
        Box::pin(self.reconcile_registration(
            &registration.tracking_number,
            registration.carrier,
            registration.started_at,
        ))
    }
    fn prepare_registration(
        &self,
        registration: Registration,
    ) -> ApiFuture<'_, Box<dyn PreparedRegistration>> {
        Box::pin(async move {
            let slot = self.request_slot().await?;
            Ok(Box::new(EasyPostRegistration {
                client: self.clone(),
                slot,
                registration,
            }) as Box<dyn PreparedRegistration>)
        })
    }
    fn retrieve<'a>(
        &'a self,
        tracker_id: &'a str,
        tracking_number: &'a str,
    ) -> ApiFuture<'a, Tracker> {
        Box::pin(self.retrieve(tracker_id, tracking_number))
    }
}

/// Owns the outbound slot across the worker's database fence. Dropping this
/// value releases pacing capacity without performing a provider write.
struct EasyPostRegistration {
    client: EasyPostClient,
    slot: OwnedMutexGuard<RequestSchedule>,
    registration: Registration,
}

impl PreparedRegistration for EasyPostRegistration {
    fn register(self: Box<Self>) -> ApiFuture<'static, Tracker> {
        Box::pin(async move {
            let expiry = shift_calendar_months(self.registration.started_at, 3)?;
            if time::OffsetDateTime::now_utc().unix_timestamp() >= expiry {
                return Err(provider_error(ErrorKind::RegistrationExpired));
            }
            self.client
                .observe(
                    "register",
                    self.client.register_in_slot(
                        &self.registration.tracking_number,
                        self.registration.carrier,
                        self.slot,
                    ),
                )
                .await
        })
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn registration_windows_use_calendar_months_and_clamp_month_end() {
        for (value, months, expected) in [
            ("2024-11-30T12:34:56Z", 3, "2025-02-28T12:34:56Z"),
            ("2023-11-30T12:34:56Z", 3, "2024-02-29T12:34:56Z"),
            ("2026-03-31T12:34:56Z", -3, "2025-12-31T12:34:56Z"),
            ("2026-05-31T12:34:56Z", -3, "2026-02-28T12:34:56Z"),
        ] {
            let parse = |value| {
                time::OffsetDateTime::parse(value, &time::format_description::well_known::Rfc3339)
                    .unwrap()
                    .unix_timestamp()
            };
            assert_eq!(
                crate::shipping_api::easypost::shift_calendar_months(parse(value), months).unwrap(),
                parse(expected)
            );
        }
    }
}
