//! Outbound EasyPost polling. Claims survive restarts; network calls never hold
//! a database transaction, and a generation/token fence rejects stale results.

use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use reqwest::{Client, RequestBuilder, Response, Url};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::{FromRow, SqlitePool};
use tokio::sync::Mutex;
use tokio::time::Instant;
use uuid::Uuid;

use crate::carrier::Carrier;
use crate::error::Error;
use crate::models::{
    CarrierEvent, CarrierTracking, EventKind, ProviderValue, Status, provider_optional,
};

const LEASE_SECONDS: i64 = 60;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
const MAX_BODY_BYTES: usize = 256 * 1024;
const MAX_RETRY_SECONDS: i64 = 6 * 60 * 60;

/// Only these safe errors may reach persisted sync state. In particular, never
/// retain reqwest errors, provider error bodies, or authentication headers.
#[derive(Debug, thiserror::Error)]
pub enum SyncError {
    #[error("invalid EasyPost client configuration")]
    Configuration,
    #[error("EasyPost request timed out")]
    Timeout,
    #[error("EasyPost connection failed")]
    Connection,
    #[error("EasyPost returned HTTP {status}")]
    Http {
        status: u16,
        retry_after: Option<i64>,
    },
    #[error("EasyPost requests are paused; retry in {retry_after} seconds")]
    Throttled { retry_after: i64 },
    #[error(transparent)]
    Rejected(#[from] TrackerRejection),
    #[error("{rejection}")]
    ProviderRejected {
        rejection: TrackerRejection,
        status: u16,
        retry_after: Option<i64>,
    },
    #[error(
        "EasyPost returned multiple possible trackers for this registration. Select the correct carrier before retrying; no new tracker was created."
    )]
    RegistrationAmbiguous,
    #[error(
        "The original EasyPost registration could not be confirmed within its three-month duplicate-protection window. Review the tracker in EasyPost before registering it again; no new tracker was created."
    )]
    RegistrationExpired,
    #[error("EasyPost response exceeded the size limit")]
    ResponseTooLarge,
    #[error("EasyPost returned an invalid tracker")]
    InvalidResponse,
    #[error("EasyPost returned an invalid connection-check response")]
    InvalidConnectionResponse,
}

impl SyncError {
    pub fn error_code(&self) -> &'static str {
        match self {
            Self::Rejected(rejection) | Self::ProviderRejected { rejection, .. } => {
                rejection.error_code()
            }
            Self::RegistrationAmbiguous => "registration_ambiguous",
            Self::RegistrationExpired => "registration_expired",
            Self::Configuration => "provider_configuration",
            Self::Timeout => "provider_timeout",
            Self::Connection => "provider_connection",
            Self::Http {
                status: 401 | 403, ..
            } => "tracking_provider_credentials",
            Self::Http { status: 429, .. } => "tracking_verification_rate_limited",
            Self::Http { .. } => "provider_http",
            Self::Throttled { .. } => "provider_throttled",
            Self::ResponseTooLarge => "provider_response_too_large",
            Self::InvalidResponse | Self::InvalidConnectionResponse => "provider_invalid_response",
        }
    }

    pub fn provider_status(&self) -> Option<u16> {
        match self {
            Self::Http { status, .. } | Self::ProviderRejected { status, .. } => Some(*status),
            _ => None,
        }
    }

    pub fn provider_error_code(&self) -> Option<&'static str> {
        match self {
            Self::ProviderRejected { rejection, .. } => Some(rejection.provider_error_code()),
            _ => None,
        }
    }

    fn retry_after(&self) -> Option<i64> {
        match self {
            Self::Http { retry_after, .. } | Self::ProviderRejected { retry_after, .. } => {
                *retry_after
            }
            Self::Throttled { retry_after } => Some(*retry_after),
            _ => None,
        }
    }

    fn set_retry_after(&mut self, value: i64) {
        match self {
            Self::Http { retry_after, .. } | Self::ProviderRejected { retry_after, .. } => {
                *retry_after = Some(value)
            }
            _ => {}
        }
    }

    fn rejects_registration(&self) -> bool {
        matches!(
            self,
            Self::Rejected(
                TrackerRejection::NoCarrierForCode
                    | TrackerRejection::CarrierCodeMismatch
                    | TrackerRejection::MultipleCarriersForCode
                    | TrackerRejection::InvalidParams
            ) | Self::ProviderRejected {
                rejection: TrackerRejection::NoCarrierForCode
                    | TrackerRejection::CarrierCodeMismatch
                    | TrackerRejection::MultipleCarriersForCode
                    | TrackerRejection::InvalidParams,
                ..
            } | Self::RegistrationAmbiguous
                | Self::RegistrationExpired
        )
    }
}

/// Documented tracker creation failures. Provider free text and nested errors
/// are intentionally ignored so rejected input cannot leak private data.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, thiserror::Error)]
pub enum TrackerRejection {
    #[serde(rename = "TRACKER.NO_CARRIER_FOR_CODE")]
    #[error(
        "EasyPost could not identify a supported carrier. Check the tracking number or specify its carrier."
    )]
    NoCarrierForCode,
    #[serde(rename = "TRACKER.CARRIER_CODE_MISMATCH")]
    #[error(
        "The tracking number does not match the selected carrier. Check the number and carrier."
    )]
    CarrierCodeMismatch,
    #[serde(rename = "TRACKER.MULTIPLE_CARRIERS_FOR_CODE")]
    #[error("The tracking number matches multiple carriers. Specify its carrier.")]
    MultipleCarriersForCode,
    #[serde(rename = "TRACKER.INVALID_PARAMS")]
    #[error("EasyPost could not accept this tracking number and carrier. Check both values.")]
    InvalidParams,
    #[serde(rename = "TRACKER.INVALID_TEST_CODE")]
    #[error(
        "This EasyPost test key accepts only EasyPost test tracking numbers. Use a production key for real shipments."
    )]
    InvalidTestCode,
    #[serde(rename = "TRACKER.NO_CARRIER_ACCOUNT")]
    #[error(
        "The EasyPost account needs a carrier account for this carrier. Configure it in EasyPost and try again."
    )]
    NoCarrierAccount,
    #[serde(rename = "CREDENTIALS_NOT_FOUND")]
    #[error(
        "EasyPost has no credentials for this carrier. Configure this carrier in EasyPost, then retry. This does not mean the tracking number is invalid."
    )]
    CarrierCredentialsMissing,
}

impl TrackerRejection {
    pub fn error_code(self) -> &'static str {
        match self {
            Self::NoCarrierForCode => "tracking_unsupported",
            Self::CarrierCodeMismatch => "tracking_carrier_mismatch",
            Self::MultipleCarriersForCode => "tracking_carrier_ambiguous",
            Self::InvalidParams => "tracking_invalid",
            Self::InvalidTestCode => "tracking_test_key",
            Self::NoCarrierAccount => "tracking_carrier_account_required",
            Self::CarrierCredentialsMissing => "tracking_carrier_credentials_required",
        }
    }

    pub fn provider_error_code(self) -> &'static str {
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

/// Carrier text and locations are retained for the tracking timeline, but must
/// never be included in operational logs or provider errors.
#[derive(Debug, Deserialize)]
#[serde(from = "ProviderTracker")]
pub struct Tracker {
    pub id: String,
    pub tracking_code: String,
    pub carrier: Carrier,
    pub status: Status,
    pub public_url: Option<String>,
    pub carrier_tracking: CarrierTracking,
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
        let url = Url::parse(base_url).map_err(|_| SyncError::Configuration)?;
        let loopback = url
            .host_str()
            .and_then(|host| host.trim_matches(['[', ']']).parse::<IpAddr>().ok())
            .is_some_and(|address| address.is_loopback());
        if !loopback || !matches!(url.scheme(), "http" | "https") {
            return Err(SyncError::Configuration);
        }
        Self::build(api_key.into(), base_url)
    }

    fn build(api_key: String, base_url: &str) -> Result<Self, SyncError> {
        let base = Url::parse(base_url).map_err(|_| SyncError::Configuration)?;
        if api_key.trim().is_empty()
            || !base.username().is_empty()
            || base.password().is_some()
            || base.query().is_some()
            || base.fragment().is_some()
            || !base.path().ends_with('/')
        {
            return Err(SyncError::Configuration);
        }
        let client = Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(concat!("shipments/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|_| SyncError::Configuration)?;
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
        let (outcome, error_code, provider_status, retry_after) = match error {
            None => ("success", None, None, None),
            Some(SyncError::Throttled { retry_after }) => (
                "throttled",
                Some("provider_throttled"),
                None,
                Some(*retry_after),
            ),
            Some(SyncError::Http {
                status: 429,
                retry_after,
            }) => (
                "rate_limited",
                Some("provider_rate_limited"),
                Some(429),
                *retry_after,
            ),
            Some(SyncError::Http {
                status,
                retry_after,
            }) => ("error", Some("provider_http"), Some(*status), *retry_after),
            Some(SyncError::Rejected(rejection)) => {
                ("rejected", Some(rejection.error_code()), None, None)
            }
            Some(SyncError::ProviderRejected {
                rejection,
                status,
                retry_after,
            }) => (
                "rejected",
                Some(rejection.error_code()),
                Some(*status),
                *retry_after,
            ),
            Some(error @ (SyncError::RegistrationAmbiguous | SyncError::RegistrationExpired)) => {
                ("rejected", Some(error.error_code()), None, None)
            }
            Some(error) => (
                "error",
                Some(match error {
                    SyncError::Configuration => "provider_configuration",
                    SyncError::Timeout => "provider_timeout",
                    SyncError::Connection => "provider_connection",
                    SyncError::ResponseTooLarge => "provider_response_too_large",
                    _ => "provider_invalid_response",
                }),
                None,
                None,
            ),
        };
        slog::info!(self.log, "provider_request";
            "request_id" => request_id, "operation" => operation, "outcome" => outcome,
            "error_code" => error_code, "provider_status" => provider_status,
            "provider_error_code" => error.and_then(SyncError::provider_error_code),
            "retry_after_seconds" => retry_after, "duration_ms" => started.elapsed().as_millis());
        result
    }

    // All clones share one outbound slot, including API verification and health
    // checks. Reject contention instead of holding an unbounded request queue or
    // waiting out a provider cooldown inside a shipment's lease.
    async fn send(&self, request: RequestBuilder) -> Result<Vec<u8>, SyncError> {
        let mut schedule = self
            .requests
            .try_lock()
            .map_err(|_| SyncError::Throttled { retry_after: 1 })?;
        if let Some(until) = schedule.cooldown.filter(|until| *until > Instant::now()) {
            let remaining = until.saturating_duration_since(Instant::now());
            return Err(SyncError::Throttled {
                retry_after: i64::try_from(remaining.as_secs()).unwrap_or(MAX_RETRY_SECONDS) + 1,
            });
        }
        tokio::time::sleep_until(schedule.next_start).await;
        schedule.next_start = Instant::now() + Duration::from_secs(1);
        let result = match request.send().await {
            Ok(response) => Self::read_body(response).await,
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
                .map_err(|_| SyncError::Configuration)?;
            let request = self
                .client
                .get(url)
                .query(&[("page_size", 1_u8)])
                .basic_auth(&self.api_key, Some(""));
            let page: TrackerPage = serde_json::from_slice(&self.send(request).await?)
                .map_err(|_| SyncError::InvalidConnectionResponse)?;
            if page.trackers.len() > 1 {
                return Err(SyncError::InvalidConnectionResponse);
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
            let url = self
                .base
                .join("trackers")
                .map_err(|_| SyncError::Configuration)?;
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
            let tracker = Self::read_tracker(&self.send(request).await?, tracking_code)?;
            if carrier.is_some_and(|carrier| carrier != tracker.carrier) {
                return Err(TrackerRejection::CarrierCodeMismatch.into());
            }
            Ok(tracker)
        })
        .await
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
                .map_err(|_| SyncError::Configuration)?
                .format(&time::format_description::well_known::Rfc3339)
                .map_err(|_| SyncError::Configuration)?;
            let end = time::OffsetDateTime::from_unix_timestamp(now.saturating_add(60))
                .map_err(|_| SyncError::Configuration)?
                .format(&time::format_description::well_known::Rfc3339)
                .map_err(|_| SyncError::Configuration)?;
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
                .map_err(|_| SyncError::Configuration)?;
            let request = self
                .client
                .get(url)
                .query(&query)
                .basic_auth(&self.api_key, Some(""));
            #[derive(Deserialize)]
            struct Candidates {
                trackers: Vec<Tracker>,
                has_more: bool,
            }
            let page: Candidates = serde_json::from_slice(&self.send(request).await?)
                .map_err(|_| SyncError::InvalidResponse)?;
            if page.has_more || page.trackers.len() > 1 {
                return Err(SyncError::RegistrationAmbiguous);
            }
            let tracker = page
                .trackers
                .into_iter()
                .next()
                .map(|tracker| {
                    let tracker = Self::validate_tracker(tracker, tracking_code)?;
                    if carrier.is_some_and(|carrier| carrier != tracker.carrier) {
                        return Err(SyncError::InvalidResponse);
                    }
                    Ok(tracker)
                })
                .transpose()?;
            if tracker.is_none() && now >= shift_calendar_months(first_attempt, 3)? {
                return Err(SyncError::RegistrationExpired);
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
                return Err(SyncError::InvalidResponse);
            }
            let mut url = self
                .base
                .join("trackers/")
                .map_err(|_| SyncError::Configuration)?;
            url.path_segments_mut()
                .map_err(|_| SyncError::Configuration)?
                .pop_if_empty()
                .push(tracker_id);
            let request = self.client.get(url).basic_auth(&self.api_key, Some(""));
            let tracker = Self::read_tracker(&self.send(request).await?, tracking_code)?;
            if tracker.id != tracker_id {
                return Err(SyncError::InvalidResponse);
            }
            Ok(tracker)
        })
        .await
    }

    fn read_tracker(body: &[u8], tracking_code: &str) -> Result<Tracker, SyncError> {
        let tracker = serde_json::from_slice(body).map_err(|_| SyncError::InvalidResponse)?;
        Self::validate_tracker(tracker, tracking_code)
    }

    fn validate_tracker(mut tracker: Tracker, tracking_code: &str) -> Result<Tracker, SyncError> {
        if !valid_tracker_id(&tracker.id)
            || tracker.tracking_code != tracking_code
            || tracker.status == Status::Expected
        {
            return Err(SyncError::InvalidResponse);
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
            let error = SyncError::Http {
                status: status.as_u16(),
                retry_after,
            };
            Some(error)
        } else {
            None
        };
        if response
            .content_length()
            .is_some_and(|length| length > MAX_BODY_BYTES as u64)
        {
            return Err(http_error.unwrap_or(SyncError::ResponseTooLarge));
        }
        let mut body = Vec::new();
        loop {
            let chunk = match response.chunk().await {
                Ok(Some(chunk)) => chunk,
                Ok(None) => break,
                Err(error) => return Err(http_error.unwrap_or_else(|| redact_request_error(error))),
            };
            if chunk.len() > MAX_BODY_BYTES.saturating_sub(body.len()) {
                return Err(http_error.unwrap_or(SyncError::ResponseTooLarge));
            }
            body.extend_from_slice(&chunk);
        }
        if let Some(http_error) = http_error {
            let retry_after = http_error.retry_after();
            return Err(serde_json::from_slice::<TrackerError>(&body)
                .map(|parsed| SyncError::ProviderRejected {
                    rejection: parsed.error.code,
                    status: status.as_u16(),
                    retry_after,
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
        SyncError::Timeout
    } else {
        SyncError::Connection
    }
}

// EasyPost documents a calendar-month deduplication window. Clamp month-end
// days explicitly rather than approximating three months with ninety days.
fn shift_calendar_months(timestamp: i64, months: i32) -> Result<i64, SyncError> {
    let at = time::OffsetDateTime::from_unix_timestamp(timestamp)
        .map_err(|_| SyncError::Configuration)?;
    let ordinal = at
        .year()
        .checked_mul(12)
        .and_then(|year| year.checked_add(i32::from(u8::from(at.month())) - 1))
        .and_then(|month| month.checked_add(months))
        .ok_or(SyncError::Configuration)?;
    let year = ordinal.div_euclid(12);
    let month = time::Month::try_from(
        u8::try_from(ordinal.rem_euclid(12) + 1).map_err(|_| SyncError::Configuration)?,
    )
    .map_err(|_| SyncError::Configuration)?;
    for day in (1..=at.day()).rev() {
        if let Ok(date) = time::Date::from_calendar_date(year, month, day) {
            return Ok(date.with_time(at.time()).assume_utc().unix_timestamp());
        }
    }
    Err(SyncError::Configuration)
}

pub(crate) fn poll_delay(status: Status, unchanged_seconds: i64, shipment_id: &str) -> Option<i64> {
    let base = if status.is_terminal() {
        return None;
    } else if status == Status::OutForDelivery {
        5 * 60
    } else if matches!(status, Status::Unknown | Status::PreTransit)
        && unchanged_seconds >= 7 * 86400
    {
        6 * 60 * 60
    } else if matches!(status, Status::Unknown | Status::PreTransit)
        && unchanged_seconds >= 2 * 86400
    {
        2 * 60 * 60
    } else {
        30 * 60
    };
    // A stable per-shipment offset spreads batches out without new random state.
    // Only add delay, so jitter never increases the request rate.
    let hash = Sha256::digest(shipment_id.as_bytes());
    Some(base + i64::from(u16::from_be_bytes([hash[0], hash[1]])) % (base / 10 + 1))
}

#[derive(FromRow)]
struct Claim {
    id: String,
    tracking_generation: i64,
    tracking_number: String,
    tracking_carrier: Option<String>,
    tracker_id: Option<String>,
    registration_started_at: Option<i64>,
    lease_token: String,
}

#[derive(FromRow)]
struct CurrentSync {
    carrier_status: Option<String>,
    tracking_carrier: Option<String>,
    override_status: Option<String>,
    attempts: i64,
    status_age: i64,
    carrier_tracking: String,
}

#[derive(Clone)]
pub struct SyncWorker {
    pool: SqlitePool,
    client: EasyPostClient,
}

impl SyncWorker {
    pub fn new(pool: SqlitePool, client: EasyPostClient) -> Self {
        Self { pool, client }
    }

    /// Attempt one due shipment. Multiple workers can tick concurrently. A
    /// dropped tick leaves a durable lease that another worker can reclaim.
    pub async fn tick(&self) -> Result<bool, Error> {
        if self.client.blocked() {
            return Ok(false);
        }
        let claim = sqlx::query_as::<_, Claim>(
            "UPDATE shipments SET lease_token = ?, lease_until = unixepoch() + ?, last_attempt_at = unixepoch()
             WHERE id = (
                 SELECT id FROM shipments
                 WHERE archived = 0 AND tracking_number IS NOT NULL
                   AND registration_state != 'rejected'
                   AND next_attempt_at <= unixepoch()
                   AND (lease_until IS NULL OR lease_until <= unixepoch())
                 ORDER BY next_attempt_at, id LIMIT 1
             )
             RETURNING id, tracking_generation, tracking_number, tracking_carrier,
                       tracker_id, registration_started_at, lease_token",
        )
        .bind(Uuid::now_v7().to_string())
        .bind(LEASE_SECONDS)
        .fetch_optional(&self.pool)
        .await?;
        let Some(claim) = claim else { return Ok(false) };

        let mut client = self.client.with_request_id(&claim.lease_token);
        client.log = client.log.new(slog::o!("shipment_id" => claim.id.clone()));
        let carrier = claim
            .tracking_carrier
            .as_deref()
            .map(str::parse)
            .transpose()
            .map_err(|error: crate::carrier::UnsupportedCarrier| {
                Error::Database(sqlx::Error::Decode(Box::new(error)))
            })?;
        let observation = match &claim.tracker_id {
            Some(id) => client.retrieve(id, &claim.tracking_number).await,
            None => {
                // Commit the POST intent before crossing the network boundary.
                // Cancellation or restart after this point must reconcile first.
                let started = sqlx::query_scalar::<_, i64>(
                    "UPDATE shipments SET registration_started_at=coalesce(registration_started_at,unixepoch())
                     WHERE id=? AND tracking_generation=? AND lease_token=?
                       AND lease_until>unixepoch() AND archived=0 AND next_attempt_at IS NOT NULL
                     RETURNING registration_started_at",
                ).bind(&claim.id).bind(claim.tracking_generation).bind(&claim.lease_token)
                    .fetch_optional(&self.pool).await?;
                let Some(started) = started else {
                    self.release_cancelled_registration(&claim).await?;
                    return Ok(true);
                };
                if claim.registration_started_at.is_some() {
                    match client
                        .reconcile_registration(&claim.tracking_number, carrier, started)
                        .await
                    {
                        Ok(Some(tracker)) => Ok(tracker),
                        Ok(None) => {
                            // The lookup crossed a network boundary. Do not create
                            // tracking for a record that was edited or archived meanwhile.
                            let owned = sqlx::query_scalar::<_, bool>(
                                "SELECT EXISTS(SELECT 1 FROM shipments WHERE id=? AND tracking_generation=? AND lease_token=? AND lease_until>unixepoch() AND archived=0 AND next_attempt_at IS NOT NULL)",
                            ).bind(&claim.id).bind(claim.tracking_generation).bind(&claim.lease_token)
                                .fetch_one(&self.pool).await?;
                            if !owned {
                                self.release_cancelled_registration(&claim).await?;
                                return Ok(true);
                            }
                            match shift_calendar_months(started, 3) {
                                Ok(expiry)
                                    if time::OffsetDateTime::now_utc().unix_timestamp()
                                        < expiry =>
                                {
                                    client.register(&claim.tracking_number, carrier).await
                                }
                                Ok(_) => Err(SyncError::RegistrationExpired),
                                Err(error) => Err(error),
                            }
                        }
                        Err(error) => Err(error),
                    }
                } else {
                    client.register(&claim.tracking_number, carrier).await
                }
            }
        };
        self.complete(&claim, observation).await?;
        Ok(true)
    }

    async fn release_cancelled_registration(&self, claim: &Claim) -> Result<(), Error> {
        // A terminal correction can cancel the schedule while a read-only
        // reconciliation is in flight. Release only this still-owned lease so
        // a later explicit refresh can queue its single permitted probe.
        sqlx::query("UPDATE shipments SET lease_token=NULL,lease_until=NULL WHERE id=? AND tracking_generation=? AND lease_token=? AND lease_until>unixepoch() AND archived=0 AND next_attempt_at IS NULL")
            .bind(&claim.id).bind(claim.tracking_generation).bind(&claim.lease_token)
            .execute(&self.pool).await?;
        Ok(())
    }

    async fn complete(
        &self,
        claim: &Claim,
        observation: Result<Tracker, SyncError>,
    ) -> Result<(), Error> {
        let mut transaction = self.pool.begin().await?;
        // Acquire the SQLite write lock and verify the fence in one statement.
        // Subsequent statements cannot race another claim or a user mutation.
        let current = sqlx::query_as::<_, CurrentSync>(
            "UPDATE shipments SET lease_until = lease_until
             WHERE id = ? AND tracking_generation = ? AND lease_token = ?
               AND lease_until > unixepoch() AND archived = 0
             RETURNING carrier_status, tracking_carrier, override_status, attempts, carrier_tracking,
                       max(0, unixepoch() - coalesce(unixepoch(status_since), unixepoch())) AS status_age",
        )
        .bind(&claim.id)
        .bind(claim.tracking_generation)
        .bind(&claim.lease_token)
        .fetch_optional(&mut *transaction)
        .await?;
        let Some(current) = current else {
            transaction.rollback().await?;
            return Ok(());
        };
        let override_status: Option<Status> = current
            .override_status
            .as_deref()
            .map(serde_json::from_str)
            .transpose()?;
        let override_terminal = override_status.is_some_and(Status::is_terminal);
        let previous_status: Option<Status> = current
            .carrier_status
            .as_deref()
            .map(serde_json::from_str)
            .transpose()?;
        match observation {
            Ok(tracker) => {
                let status = serde_json::to_string(&tracker.status)?;
                let carrier_key = tracker.carrier.as_db_key();
                let carrier_changed =
                    current.tracking_carrier.as_deref() != Some(carrier_key.as_str());
                // Carrier is part of the tracking identity. A canonical carrier
                // change must invalidate edits that read the earlier identity.
                let mut carrier_tracking: CarrierTracking =
                    serde_json::from_str(&current.carrier_tracking)?;
                let history_changed = carrier_tracking.merge(&tracker.carrier_tracking);
                let changed = current.carrier_status.as_deref() != Some(&status)
                    || carrier_changed
                    || history_changed;
                let next_delay = if override_terminal {
                    None
                } else {
                    poll_delay(
                        tracker.status,
                        if previous_status == Some(tracker.status) {
                            current.status_age
                        } else {
                            0
                        },
                        &claim.id,
                    )
                };
                sqlx::query(
                    "UPDATE shipments SET tracker_id = ?, carrier_status = ?,
                         tracking_carrier = ?, tracking_url = ?, carrier_tracking = ?,
                         revision = revision + ?,
                         updated_at = CASE WHEN ? THEN strftime('%Y-%m-%dT%H:%M:%fZ', 'now') ELSE updated_at END,
                         last_success_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now'),
                         last_attempt_at = unixepoch(),
                         status_since = CASE WHEN carrier_status = ? THEN coalesce(status_since, strftime('%Y-%m-%dT%H:%M:%fZ', 'now')) ELSE strftime('%Y-%m-%dT%H:%M:%fZ', 'now') END,
                         registration_state = 'registered',
                         last_error = NULL, last_error_code = NULL, last_provider_status = NULL, last_provider_error_code = NULL,
                         attempts = 0, next_attempt_at = unixepoch() + ?,
                         lease_token = NULL, lease_until = NULL
                     WHERE id = ?",
                )
                .bind(&tracker.id)
                .bind(&status)
                .bind(&carrier_key)
                .bind(&tracker.public_url)
                .bind(serde_json::to_string(&carrier_tracking)?)
                .bind(i64::from(changed))
                .bind(changed)
                .bind(&status)
                .bind(next_delay)
                .bind(&claim.id)
                .execute(&mut *transaction)
                .await?;
                if changed {
                    let id = Uuid::parse_str(&claim.id)
                        .map_err(|error| Error::Database(sqlx::Error::Decode(Box::new(error))))?;
                    let snapshot = crate::store::Store::get_in(&mut transaction, id).await?;
                    let previous_status =
                        serde_json::to_string(&previous_status.unwrap_or_default())?;
                    let detail = if carrier_changed {
                        format!(
                            "Carrier tracking information updated: carrier {} -> {}; status {previous_status} -> {status}",
                            current.tracking_carrier.as_deref().unwrap_or("unspecified"),
                            tracker.carrier,
                        )
                    } else if previous_status == status {
                        "Carrier tracking history or delivery details updated".into()
                    } else {
                        format!("Carrier status: {previous_status} -> {status}")
                    };
                    sqlx::query("INSERT INTO events (shipment_id, kind, actor_id, detail, snapshot) VALUES (?, ?, 'easypost', ?, ?)")
                        .bind(&claim.id)
                        .bind(serde_json::to_string(&EventKind::CarrierUpdated)?)
                        .bind(detail)
                        .bind(serde_json::to_string(&snapshot)?)
                        .execute(&mut *transaction)
                        .await?;
                }
            }
            Err(SyncError::Throttled { retry_after }) => {
                // Another API request took the shared slot after this claim.
                // Reschedule without counting a provider failure or changing
                // the user's last error, and release the claim normally.
                sqlx::query("UPDATE shipments SET next_attempt_at = CASE WHEN next_attempt_at IS NOT NULL THEN unixepoch() + ? END, lease_token = NULL, lease_until = NULL WHERE id = ?")
                    .bind(retry_after).bind(&claim.id).execute(&mut *transaction).await?;
            }
            Err(error) => {
                let attempts = current.attempts.saturating_add(1);
                let exponential =
                    60_i64.saturating_mul(2_i64.saturating_pow(
                        u32::try_from(attempts.saturating_sub(1)).unwrap_or(u32::MAX),
                    ));
                let retry_after = error.retry_after().unwrap_or(0);
                // Explicit refresh permits one probe after a terminal status,
                // including when that probe fails. It must not restart polling.
                let terminal =
                    override_terminal || previous_status.is_some_and(Status::is_terminal);
                let rejected = claim.tracker_id.is_none() && error.rejects_registration();
                let delay = (!terminal && !rejected)
                    .then_some(exponential.clamp(60, MAX_RETRY_SECONDS).max(retry_after));
                sqlx::query(
                    "UPDATE shipments SET last_error = ?, last_error_code = ?, last_provider_status = ?, last_provider_error_code = ?,
                         registration_state = ?, attempts = ?, last_attempt_at = unixepoch(),
                         next_attempt_at = unixepoch() + ?, lease_token = NULL, lease_until = NULL
                     WHERE id = ?",
                )
                .bind(error.to_string())
                .bind(error.error_code())
                .bind(error.provider_status().map(i64::from))
                .bind(error.provider_error_code())
                .bind(if claim.tracker_id.is_some() { "registered" } else if rejected { "rejected" } else { "pending" })
                .bind(attempts)
                .bind(delay)
                .bind(&claim.id)
                .execute(&mut *transaction)
                .await?;
            }
        }
        transaction.commit().await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        models::{CreateShipment, SetOverride, Status, Tracking},
        store::Store,
        sync::{Claim, EasyPostClient, SyncError, SyncWorker, poll_delay},
    };

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
                crate::sync::shift_calendar_months(parse(value), months).unwrap(),
                parse(expected)
            );
        }
    }

    #[test]
    fn polling_offsets_are_stable_bounded_and_spread_a_batch() {
        let delays: Vec<_> = (0..20)
            .map(|index| poll_delay(Status::InTransit, 0, &format!("shipment-{index}")).unwrap())
            .collect();
        assert!(delays.iter().all(|delay| (1800..=1980).contains(delay)));
        assert!(delays.iter().any(|delay| *delay != delays[0]));
        assert_eq!(
            poll_delay(Status::InTransit, 0, "shipment-0"),
            Some(delays[0])
        );
        assert_eq!(poll_delay(Status::Delivered, 0, "shipment-0"), None);
    }

    #[tokio::test]
    async fn local_throttle_preserves_terminal_cancellation_and_explicit_probe() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(&directory.path().join("registry.sqlite3"))
            .await
            .unwrap();
        let shipment = store
            .create(
                CreateShipment {
                    direction: crate::models::Direction::Inbound,
                    title: "Parcel".into(),
                    merchant: None,
                    order_reference: None,
                    signature_required: None,
                    tracking: Tracking {
                        number: "TERMINAL-TRACK".into(),
                        carrier: None,
                    },
                    source: None,
                },
                "owner",
                "terminal-race",
                None,
            )
            .await
            .unwrap();
        let claim = Claim {
            id: shipment.id.to_string(),
            tracking_generation: 0,
            tracking_number: shipment.tracking.number,
            tracking_carrier: None,
            tracker_id: None,
            registration_started_at: None,
            lease_token: "test-claim".into(),
        };
        sqlx::query("UPDATE shipments SET lease_token=?,lease_until=unixepoch()+60 WHERE id=?")
            .bind(&claim.lease_token)
            .bind(&claim.id)
            .execute(store.pool())
            .await
            .unwrap();
        let terminal = store
            .set_override(
                shipment.id,
                shipment.revision,
                Some(SetOverride {
                    status: Status::Delivered,
                    reason: "Received".into(),
                }),
                "owner",
            )
            .await
            .unwrap();
        let worker = SyncWorker::new(
            store.pool().clone(),
            EasyPostClient::new("unused-key").unwrap(),
        );
        worker
            .complete(&claim, Err(SyncError::Throttled { retry_after: 1 }))
            .await
            .unwrap();
        assert!(
            store
                .get(shipment.id)
                .await
                .unwrap()
                .sync
                .next_attempt_at
                .is_none()
        );

        store
            .refresh(shipment.id, terminal.revision, "owner")
            .await
            .unwrap();
        sqlx::query("UPDATE shipments SET lease_token=?,lease_until=unixepoch()+60 WHERE id=?")
            .bind(&claim.lease_token)
            .bind(&claim.id)
            .execute(store.pool())
            .await
            .unwrap();
        worker
            .complete(&claim, Err(SyncError::Throttled { retry_after: 1 }))
            .await
            .unwrap();
        let deferred = store.get(shipment.id).await.unwrap();
        assert!(deferred.sync.next_attempt_at.is_some());
        assert_eq!(deferred.sync.attempts, 0);
        assert!(deferred.sync.last_error.is_none());
    }
}
