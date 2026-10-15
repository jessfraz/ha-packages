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

use crate::error::Error;
use crate::models::{EventKind, Status};

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
    #[error("EasyPost response exceeded the size limit")]
    ResponseTooLarge,
    #[error("EasyPost returned an invalid tracker")]
    InvalidResponse,
    #[error("EasyPost returned an invalid connection-check response")]
    InvalidConnectionResponse,
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

/// The subset of the tracker contract used by the registry. Carrier-provided
/// free text and location data are deliberately neither stored nor logged.
#[derive(Debug, Deserialize)]
pub struct Tracker {
    pub id: String,
    pub tracking_code: String,
    pub carrier: String,
    pub status: Status,
    #[serde(default)]
    pub public_url: Option<String>,
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
    carrier: Option<&'a str>,
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
            Err(SyncError::Http {
                status: 429,
                retry_after,
            }) => {
                schedule.rate_limits = schedule.rate_limits.saturating_add(1);
                let delay = 60_i64
                    .saturating_mul(2_i64.saturating_pow(schedule.rate_limits.saturating_sub(1)))
                    .clamp(60, MAX_RETRY_SECONDS)
                    .max(retry_after.unwrap_or(0));
                schedule.cooldown =
                    Some(Instant::now() + Duration::from_secs(delay.unsigned_abs()));
                Err(SyncError::Http {
                    status: 429,
                    retry_after: Some(delay),
                })
            }
            Err(SyncError::Http {
                status,
                retry_after: Some(delay),
            }) => {
                schedule.cooldown =
                    Some(Instant::now() + Duration::from_secs(delay.unsigned_abs()));
                Err(SyncError::Http {
                    status,
                    retry_after: Some(delay),
                })
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
        carrier: Option<&str>,
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
            if carrier.is_some_and(|carrier| !carrier.eq_ignore_ascii_case(&tracker.carrier)) {
                return Err(TrackerRejection::CarrierCodeMismatch.into());
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
        let mut tracker: Tracker =
            serde_json::from_slice(body).map_err(|_| SyncError::InvalidResponse)?;
        if !valid_tracker_id(&tracker.id)
            || tracker.tracking_code != tracking_code
            || tracker.carrier.trim().is_empty()
            || tracker.carrier.len() > 100
            || tracker.carrier.chars().any(char::is_control)
            || tracker.status == Status::Expected
        {
            return Err(SyncError::InvalidResponse);
        }
        tracker.carrier = tracker.carrier.trim().to_ascii_lowercase();
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
            if !matches!(status.as_u16(), 400 | 422) {
                return Err(error);
            }
            Some(error)
        } else {
            None
        };
        if response
            .content_length()
            .is_some_and(|length| length > MAX_BODY_BYTES as u64)
        {
            return Err(SyncError::ResponseTooLarge);
        }
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(redact_request_error)? {
            if chunk.len() > MAX_BODY_BYTES.saturating_sub(body.len()) {
                return Err(SyncError::ResponseTooLarge);
            }
            body.extend_from_slice(&chunk);
        }
        if let Some(error) = http_error {
            return Err(serde_json::from_slice::<TrackerError>(&body)
                .map(|error| SyncError::Rejected(error.error.code))
                .unwrap_or(error));
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
    lease_token: String,
}

#[derive(FromRow)]
struct CurrentSync {
    carrier_status: Option<String>,
    tracking_carrier: Option<String>,
    override_status: Option<String>,
    attempts: i64,
    status_age: i64,
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
                   AND next_attempt_at <= unixepoch()
                   AND (lease_until IS NULL OR lease_until <= unixepoch())
                 ORDER BY next_attempt_at, id LIMIT 1
             )
             RETURNING id, tracking_generation, tracking_number, tracking_carrier,
                       tracker_id, lease_token",
        )
        .bind(Uuid::now_v7().to_string())
        .bind(LEASE_SECONDS)
        .fetch_optional(&self.pool)
        .await?;
        let Some(claim) = claim else { return Ok(false) };

        let mut client = self.client.with_request_id(&claim.lease_token);
        client.log = client.log.new(slog::o!("shipment_id" => claim.id.clone()));
        let observation = match &claim.tracker_id {
            Some(id) => client.retrieve(id, &claim.tracking_number).await,
            None => {
                client
                    .register(&claim.tracking_number, claim.tracking_carrier.as_deref())
                    .await
            }
        };
        self.complete(&claim, observation).await?;
        Ok(true)
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
             RETURNING carrier_status, tracking_carrier, override_status, attempts,
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
                let carrier_changed =
                    current.tracking_carrier.as_deref() != Some(tracker.carrier.as_str());
                // Carrier is part of the tracking identity. A canonical carrier
                // change must invalidate edits that read the earlier identity.
                let changed = current.carrier_status.as_deref() != Some(&status) || carrier_changed;
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
                         tracking_carrier = ?, tracking_url = ?,
                         revision = revision + ?,
                         updated_at = CASE WHEN ? THEN strftime('%Y-%m-%dT%H:%M:%fZ', 'now') ELSE updated_at END,
                         last_success_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now'),
                         last_attempt_at = unixepoch(),
                         status_since = CASE WHEN carrier_status = ? THEN coalesce(status_since, strftime('%Y-%m-%dT%H:%M:%fZ', 'now')) ELSE strftime('%Y-%m-%dT%H:%M:%fZ', 'now') END,
                         last_error = NULL, attempts = 0, next_attempt_at = unixepoch() + ?,
                         lease_token = NULL, lease_until = NULL
                     WHERE id = ?",
                )
                .bind(&tracker.id)
                .bind(&status)
                .bind(&tracker.carrier)
                .bind(&tracker.public_url)
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
                let retry_after = match &error {
                    SyncError::Http { retry_after, .. } => retry_after.unwrap_or(0),
                    _ => 0,
                };
                // Explicit refresh permits one probe after a terminal status,
                // including when that probe fails. It must not restart polling.
                let terminal =
                    override_terminal || previous_status.is_some_and(Status::is_terminal);
                let delay = (!terminal)
                    .then_some(exponential.clamp(60, MAX_RETRY_SECONDS).max(retry_after));
                sqlx::query(
                    "UPDATE shipments SET last_error = ?, attempts = ?, last_attempt_at = unixepoch(),
                         next_attempt_at = unixepoch() + ?, lease_token = NULL, lease_until = NULL
                     WHERE id = ?",
                )
                .bind(error.to_string())
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
                    title: "Parcel".into(),
                    merchant: None,
                    order_reference: None,
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
