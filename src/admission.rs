//! Verify before admitting a shipment. Provider writes have a durable intent
//! and an expiring ownership fence, independently of public shipment records.

use std::sync::Arc;

use dropshot::{ClientErrorStatusCode, HttpError};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::FromRow;
use uuid::Uuid;

use crate::{
    models::{InputTracking, Source},
    shipping_api::{
        ShippingApi,
        error::Error as ProviderError,
        types::{Registration, Tracker},
    },
    store::Store,
};

const LEASE_SECONDS: i64 = 120;

#[derive(FromRow)]
struct Attempt {
    requested_carrier: Option<String>,
    started_at: i64,
    lease_until: Option<i64>,
    attempts: i64,
    next_attempt_at: i64,
    tracker: Option<String>,
    verified_at: Option<i64>,
    failure: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct Failure {
    code: String,
    message: String,
    rejected: bool,
}

impl Failure {
    fn from_provider(error: &ProviderError) -> Self {
        let details = error
            .provider_status()
            .map(|status| {
                let code = error
                    .provider_error_code()
                    .map(|code| format!(" ({code})"))
                    .unwrap_or_default();
                format!(
                    " Provider response: {} HTTP {status}{code}.",
                    error.provider.name()
                )
            })
            .unwrap_or_default();
        Self {
            code: error.error_code().into(),
            message: format!(
                "{error}{details} No shipment was saved or changed. Retry unchanged input with the original idempotency key; use a new key if you correct the input. Existing trackers are checked before registration."
            ),
            rejected: error.rejects_registration() || error.requires_configuration(),
        }
    }

    fn http(self, retry_after: i64) -> HttpError {
        let mut error = if self.rejected {
            HttpError::for_client_error(
                Some(self.code),
                ClientErrorStatusCode::BAD_REQUEST,
                self.message,
            )
        } else {
            let mut error = HttpError::for_unavail(Some(self.code), self.message.clone());
            error.external_message = self.message;
            error
        };
        if !self.rejected {
            error.headers = Some(Box::new(http::HeaderMap::from_iter([(
                http::header::RETRY_AFTER,
                http::HeaderValue::from_str(&retry_after.max(1).to_string())
                    .unwrap_or(http::HeaderValue::from_static("60")),
            )])));
        }
        error
    }
}

pub struct SourceClaim {
    source: Source,
    actor: String,
    key: String,
    token: String,
}

/// Reserve an imported source before a potentially billable request. Other
/// creates and source attachments respect this lease without holding SQLite
/// transactions across network calls. Cancellation releases it by expiry.
pub async fn claim_source(
    store: &Store,
    source: Option<&Source>,
    actor: &str,
    key: &str,
) -> Result<Option<SourceClaim>, HttpError> {
    let Some(source) = source else {
        return Ok(None);
    };
    let token = Uuid::now_v7().to_string();
    let mut tx = store
        .pool()
        .begin_with("BEGIN IMMEDIATE")
        .await
        .map_err(crate::error::Error::from)?;
    let exists: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM sources WHERE namespace=? AND key=?)")
            .bind(&source.namespace)
            .bind(&source.key)
            .fetch_one(&mut *tx)
            .await
            .map_err(crate::error::Error::from)?;
    if exists {
        return Err(crate::error::Error::Conflict(
            "source already belongs to another shipment".into(),
        )
        .into());
    }
    let acquired = sqlx::query("INSERT INTO admission_sources (namespace,key,actor_id,operation_key,lease_token,lease_until) VALUES (?,?,?,?,?,unixepoch()+?) ON CONFLICT(namespace,key) DO UPDATE SET actor_id=excluded.actor_id,operation_key=excluded.operation_key,lease_token=excluded.lease_token,lease_until=excluded.lease_until WHERE admission_sources.lease_until<=unixepoch()")
        .bind(&source.namespace).bind(&source.key).bind(actor).bind(key).bind(&token).bind(LEASE_SECONDS).execute(&mut *tx).await.map_err(crate::error::Error::from)?.rows_affected();
    if acquired == 0 {
        return Err(crate::error::Error::Conflict("This source is already being verified for another request. Wait for that request to finish before submitting it again.".into()).into());
    }
    tx.commit().await.map_err(crate::error::Error::from)?;
    Ok(Some(SourceClaim {
        source: source.clone(),
        actor: actor.into(),
        key: key.into(),
        token,
    }))
}

pub async fn release_source(store: &Store, claim: Option<SourceClaim>) -> Result<(), HttpError> {
    if let Some(claim) = claim {
        sqlx::query("DELETE FROM admission_sources WHERE namespace=? AND key=? AND actor_id=? AND operation_key=? AND lease_token=?")
            .bind(claim.source.namespace).bind(claim.source.key).bind(claim.actor).bind(claim.key).bind(claim.token)
            .execute(store.pool()).await.map_err(crate::error::Error::from)?;
    }
    Ok(())
}

fn busy(seconds: i64) -> HttpError {
    Failure {
        code: "tracking_verification_pending".into(),
        message: "Tracking verification is already in progress or waiting to retry. No shipment was saved. Retry the same request and idempotency key after the Retry-After interval.".into(),
        rejected: false,
    }.http(seconds)
}

/// Bind admission retries before any provider call, including unsuccessful ones.
pub async fn bind_request(
    store: &Store,
    actor: &str,
    key: &str,
    input: &impl Serialize,
) -> Result<(), HttpError> {
    let fingerprint = format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(input).map_err(crate::error::Error::from)?)
    );
    let saved: String = sqlx::query_scalar(
        "INSERT INTO admission_requests (actor_id,operation_key,fingerprint) VALUES (?,?,?) ON CONFLICT (actor_id,operation_key) DO UPDATE SET operation_key=excluded.operation_key RETURNING fingerprint",
    ).bind(actor).bind(key).bind(&fingerprint).fetch_one(store.pool()).await.map_err(crate::error::Error::from)?;
    if saved != fingerprint {
        return Err(crate::error::Error::Conflict("This request key was already used for different input. Retry the original request or use a new key for the corrected input.".into()).into());
    }
    Ok(())
}

pub async fn verify(
    store: &Store,
    provider: Option<Arc<dyn ShippingApi>>,
    tracking: &InputTracking,
    log: &slog::Logger,
    request_id: &str,
) -> Result<Tracker, HttpError> {
    let Some(provider) = provider else {
        return Err(Failure { code: "tracking_provider_unconfigured".into(),
            message: "Tracking cannot be verified because no provider is configured. Configure the EasyPost production key, then retry. No shipment was saved.".into(), rejected: false }.http(60));
    };
    let provider = provider.for_request(request_id, None);
    let provider_id = provider.id().as_str();
    let requested_carrier = tracking.carrier.map(|carrier| carrier.as_db_key());
    let token = Uuid::now_v7().to_string();
    let mut tx = store
        .pool()
        .begin_with("BEGIN IMMEDIATE")
        .await
        .map_err(crate::error::Error::from)?;
    sqlx::query(
        "INSERT OR IGNORE INTO tracking_verifications (provider,tracking_number) VALUES (?,?)",
    )
    .bind(provider_id)
    .bind(&tracking.number)
    .execute(&mut *tx)
    .await
    .map_err(crate::error::Error::from)?;
    let now: i64 = sqlx::query_scalar("SELECT unixepoch()")
        .fetch_one(&mut *tx)
        .await
        .map_err(crate::error::Error::from)?;
    let attempt = sqlx::query_as::<_, Attempt>("SELECT requested_carrier,started_at,lease_until,attempts,next_attempt_at,tracker,verified_at,failure FROM tracking_verifications WHERE provider=? AND tracking_number=?")
        .bind(provider_id).bind(&tracking.number).fetch_one(&mut *tx).await.map_err(crate::error::Error::from)?;
    let cached = attempt
        .tracker
        .as_deref()
        .map(serde_json::from_str::<Tracker>)
        .transpose()
        .map_err(crate::error::Error::from)?;
    let cached = cached.filter(|tracker| {
        tracking
            .carrier
            .is_none_or(|carrier| carrier == tracker.carrier)
    });
    if let Some(tracker) = cached
        .as_ref()
        .filter(|_| attempt.verified_at.is_some_and(|at| now - at < 60))
    {
        return Ok(tracker.clone());
    }
    if let Some(until) = attempt.lease_until.filter(|until| *until > now) {
        return Err(busy(until - now));
    }
    if requested_carrier == attempt.requested_carrier && attempt.next_attempt_at > now {
        return Err(match attempt.failure {
            Some(value) => serde_json::from_str::<Failure>(&value)
                .map_err(crate::error::Error::from)?
                .http(attempt.next_attempt_at - now),
            None => busy(attempt.next_attempt_at - now),
        });
    }
    sqlx::query("UPDATE tracking_verifications SET requested_carrier=?,lease_token=?,lease_until=unixepoch()+? WHERE provider=? AND tracking_number=?")
        .bind(requested_carrier).bind(&token).bind(LEASE_SECONDS).bind(provider_id).bind(&tracking.number)
        .execute(&mut *tx).await.map_err(crate::error::Error::from)?;
    tx.commit().await.map_err(crate::error::Error::from)?;
    let registration = Registration {
        tracking_number: tracking.number.clone(),
        carrier: tracking.carrier,
        started_at: attempt.started_at,
    };
    let result = async {
        if let Some(tracker) = cached {
            return provider.retrieve(&tracker.id, &tracking.number).await;
        }
        match provider.reconcile_registration(&registration).await? {
            Some(tracker) => Ok(tracker),
            None => {
                let prepared = provider.prepare_registration(registration).await?;
                let owned: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM tracking_verifications WHERE provider=? AND tracking_number=? AND lease_token=? AND lease_until>unixepoch())")
                    .bind(provider_id).bind(&tracking.number).bind(&token).fetch_one(store.pool()).await
                    .map_err(|_| crate::shipping_api::error::Error::new(provider.id(), crate::shipping_api::error::ErrorKind::Configuration))?;
                if !owned {
                    return Err(crate::shipping_api::error::Error::new(provider.id(), crate::shipping_api::error::ErrorKind::Throttled { retry_after: 60 }));
                }
                prepared.register().await
            }
        }
    }.await;
    match result {
        Ok(tracker) => {
            let affected = sqlx::query("UPDATE tracking_verifications SET tracker=?,verified_at=unixepoch(),failure=NULL,attempts=0,next_attempt_at=0,lease_token=NULL,lease_until=NULL WHERE provider=? AND tracking_number=? AND lease_token=? AND lease_until>unixepoch()")
                .bind(serde_json::to_string(&tracker).map_err(crate::error::Error::from)?).bind(provider_id).bind(&tracking.number).bind(&token)
                .execute(store.pool()).await.map_err(crate::error::Error::from)?.rows_affected();
            if affected == 0 {
                return Err(busy(60));
            }
            slog::info!(log, "tracking_verification"; "request_id" => request_id, "provider" => provider_id, "outcome" => "verified");
            Ok(tracker)
        }
        Err(error) => {
            let delay = (60_i64.saturating_mul(
                2_i64.saturating_pow(u32::try_from(attempt.attempts.min(9)).unwrap_or(9)),
            ))
            .min(6 * 3600)
            .max(error.retry_after().unwrap_or(0));
            let failure = Failure::from_provider(&error);
            sqlx::query("UPDATE tracking_verifications SET failure=?,attempts=attempts+1,next_attempt_at=unixepoch()+?,lease_token=NULL,lease_until=NULL WHERE provider=? AND tracking_number=? AND lease_token=?")
                .bind(serde_json::to_string(&failure).map_err(crate::error::Error::from)?).bind(delay).bind(provider_id).bind(&tracking.number).bind(&token)
                .execute(store.pool()).await.map_err(crate::error::Error::from)?;
            slog::warn!(log, "tracking_verification"; "request_id" => request_id, "provider" => provider_id, "outcome" => "not_saved", "error_code" => error.error_code(), "provider_status" => error.provider_status(), "provider_error_code" => error.provider_error_code(), "retry_after_seconds" => delay);
            Err(failure.http(delay))
        }
    }
}
