//! Outbound EasyPost polling. Claims survive restarts; network calls never hold
//! a database transaction, and a generation/token fence rejects stale results.

use std::net::IpAddr;
use std::time::Duration;

use reqwest::{Client, Response, Url};
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, SqlitePool};
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
    #[error("EasyPost response exceeded the size limit")]
    ResponseTooLarge,
    #[error("EasyPost returned an invalid tracker")]
    InvalidResponse,
}

#[derive(Clone)]
pub struct EasyPostClient {
    client: Client,
    api_key: String,
    base: Url,
}

/// The subset of the tracker contract used by the registry. Carrier-provided
/// free text and location data are deliberately neither stored nor logged.
#[derive(Debug, Deserialize)]
pub struct Tracker {
    pub id: String,
    pub tracking_code: String,
    pub status: Status,
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
        })
    }

    pub async fn register(
        &self,
        tracking_code: &str,
        carrier: Option<&str>,
    ) -> Result<Tracker, SyncError> {
        let url = self
            .base
            .join("trackers")
            .map_err(|_| SyncError::Configuration)?;
        let response = self
            .client
            .post(url)
            .basic_auth(&self.api_key, Some(""))
            .json(&RegisterTracker {
                tracker: RegisterTracking {
                    tracking_code,
                    carrier,
                },
            })
            .send()
            .await
            .map_err(redact_request_error)?;
        Self::read_tracker(response, tracking_code).await
    }

    pub async fn retrieve(
        &self,
        tracker_id: &str,
        tracking_code: &str,
    ) -> Result<Tracker, SyncError> {
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
        let response = self
            .client
            .get(url)
            .basic_auth(&self.api_key, Some(""))
            .send()
            .await
            .map_err(redact_request_error)?;
        let tracker = Self::read_tracker(response, tracking_code).await?;
        if tracker.id != tracker_id {
            return Err(SyncError::InvalidResponse);
        }
        Ok(tracker)
    }

    async fn read_tracker(
        mut response: Response,
        tracking_code: &str,
    ) -> Result<Tracker, SyncError> {
        if !response.status().is_success() {
            // Accept bounded delta-seconds. Ignore unparseable, zero, or
            // excessive advice; none of the raw header or body is persisted.
            let retry_after = response
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.parse::<i64>().ok())
                .filter(|seconds| (1..=MAX_RETRY_SECONDS).contains(seconds));
            return Err(SyncError::Http {
                status: response.status().as_u16(),
                retry_after,
            });
        }
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
        let tracker: Tracker =
            serde_json::from_slice(&body).map_err(|_| SyncError::InvalidResponse)?;
        if !valid_tracker_id(&tracker.id)
            || tracker.tracking_code != tracking_code
            || tracker.status == Status::Expected
        {
            return Err(SyncError::InvalidResponse);
        }
        Ok(tracker)
    }
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
    override_status: Option<String>,
    attempts: i64,
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
        let claim = sqlx::query_as::<_, Claim>(
            "UPDATE shipments SET lease_token = ?, lease_until = unixepoch() + ?
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

        let observation = match &claim.tracker_id {
            Some(id) => self.client.retrieve(id, &claim.tracking_number).await,
            None => {
                self.client
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
             RETURNING carrier_status, override_status, attempts",
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
                let changed = current.carrier_status.as_deref() != Some(&status);
                let next_delay = if override_terminal || tracker.status.is_terminal() {
                    None
                } else if tracker.status == Status::OutForDelivery {
                    Some(5 * 60)
                } else {
                    Some(30 * 60)
                };
                sqlx::query(
                    "UPDATE shipments SET tracker_id = ?, carrier_status = ?,
                         revision = revision + ?,
                         updated_at = CASE WHEN ? THEN strftime('%Y-%m-%dT%H:%M:%fZ', 'now') ELSE updated_at END,
                         last_success_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now'),
                         last_error = NULL, attempts = 0, next_attempt_at = unixepoch() + ?,
                         lease_token = NULL, lease_until = NULL
                     WHERE id = ?",
                )
                .bind(&tracker.id)
                .bind(&status)
                .bind(i64::from(changed))
                .bind(changed)
                .bind(next_delay)
                .bind(&claim.id)
                .execute(&mut *transaction)
                .await?;
                if changed {
                    let id = Uuid::parse_str(&claim.id)
                        .map_err(|error| Error::Database(sqlx::Error::Decode(Box::new(error))))?;
                    let snapshot = crate::store::Store::get_in(&mut transaction, id).await?;
                    sqlx::query("INSERT INTO events (shipment_id, kind, actor_id, detail, snapshot) VALUES (?, ?, 'easypost', ?, ?)")
                        .bind(&claim.id)
                        .bind(serde_json::to_string(&EventKind::CarrierUpdated)?)
                        .bind(format!(
                            "Carrier status: {} -> {status}",
                            serde_json::to_string(&previous_status.unwrap_or_default())?
                        ))
                        .bind(serde_json::to_string(&snapshot)?)
                        .execute(&mut *transaction)
                        .await?;
                }
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
                    "UPDATE shipments SET last_error = ?, attempts = ?,
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
