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
        types::{ProviderId, Registration, Tracker},
    },
    store::Store,
};

const LEASE_SECONDS: i64 = 120;

#[derive(FromRow)]
struct Attempt {
    requested_carrier: Option<String>,
    requested_recipient_url: Option<String>,
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

pub struct TrackingClaim {
    number: String,
    token: String,
}

impl TrackingClaim {
    async fn renew(&self, store: &Store) -> Result<bool, sqlx::Error> {
        Ok(sqlx::query("UPDATE admission_tracking SET lease_until=unixepoch()+? WHERE tracking_number=? AND lease_token=? AND lease_until>unixepoch()")
            .bind(LEASE_SECONDS).bind(&self.number).bind(&self.token).execute(store.pool()).await?.rows_affected() == 1)
    }
}

/// Reserve the canonical number across providers until the shipment write has
/// committed. Keep provider verification records separate for reconciliation.
pub async fn claim_tracking(store: &Store, number: &str) -> Result<TrackingClaim, HttpError> {
    let token = Uuid::now_v7().to_string();
    let acquired = sqlx::query("INSERT INTO admission_tracking (tracking_number,lease_token,lease_until) VALUES (?,?,unixepoch()+?) ON CONFLICT(tracking_number) DO UPDATE SET lease_token=excluded.lease_token,lease_until=excluded.lease_until WHERE admission_tracking.lease_until<=unixepoch()")
        .bind(number).bind(&token).bind(LEASE_SECONDS).execute(store.pool()).await.map_err(crate::error::Error::from)?.rows_affected();
    if acquired == 0 {
        return Err(busy(LEASE_SECONDS));
    }
    Ok(TrackingClaim {
        number: number.into(),
        token,
    })
}

pub async fn release_tracking(store: &Store, claim: TrackingClaim) -> Result<(), HttpError> {
    sqlx::query("DELETE FROM admission_tracking WHERE tracking_number=? AND lease_token=?")
        .bind(claim.number)
        .bind(claim.token)
        .execute(store.pool())
        .await
        .map_err(crate::error::Error::from)?;
    Ok(())
}

/// Fence the final local write as well as paid registration. Expired owners
/// cannot renew or release a successor's claim.
pub async fn renew_tracking(store: &Store, claim: &TrackingClaim) -> Result<(), HttpError> {
    if !claim
        .renew(store)
        .await
        .map_err(crate::error::Error::from)?
    {
        return Err(busy(LEASE_SECONDS));
    }
    Ok(())
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
    let fingerprint: String =
        Sha256::digest(serde_json::to_vec(input).map_err(crate::error::Error::from)?)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
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
    claim: &TrackingClaim,
    provider: Option<Arc<dyn ShippingApi>>,
    tracking: &InputTracking,
    log: &slog::Logger,
    request_id: &str,
) -> Result<Tracker, HttpError> {
    let Some(provider) = provider else {
        let expected = ProviderId::for_carrier(tracking.carrier);
        return Err(Failure { code: "tracking_provider_unconfigured".into(),
            message: format!("Tracking cannot be verified because {} is not configured. Enable this provider and retry. No shipment was saved.", expected.name()), rejected: false }.http(60));
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
    let attempt = sqlx::query_as::<_, Attempt>("SELECT requested_carrier,requested_recipient_url,started_at,lease_until,attempts,next_attempt_at,tracker,verified_at,failure FROM tracking_verifications WHERE provider=? AND tracking_number=?")
        .bind(provider_id).bind(&tracking.number).fetch_one(&mut *tx).await.map_err(crate::error::Error::from)?;
    let cached = attempt
        .tracker
        .as_deref()
        .map(serde_json::from_str::<Tracker>)
        .transpose()
        .map_err(crate::error::Error::from)?;
    let cached = cached.filter(|tracker| {
        tracker.provider == provider.id()
            && provider.supports_carrier(tracker.carrier)
            && tracker.tracking_code == tracking.number
            && attempt.requested_recipient_url == tracking.recipient_url
            && tracking
                .recipient_url
                .as_ref()
                .is_none_or(|url| tracker.public_url.as_ref() == Some(url))
            && tracking
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
    if requested_carrier == attempt.requested_carrier
        && tracking.recipient_url == attempt.requested_recipient_url
        && attempt.next_attempt_at > now
    {
        return Err(match attempt.failure {
            Some(value) => serde_json::from_str::<Failure>(&value)
                .map_err(crate::error::Error::from)?
                .http(attempt.next_attempt_at - now),
            None => busy(attempt.next_attempt_at - now),
        });
    }
    sqlx::query("UPDATE tracking_verifications SET requested_carrier=?,requested_recipient_url=?,lease_token=?,lease_until=unixepoch()+? WHERE provider=? AND tracking_number=?")
        .bind(requested_carrier).bind(&tracking.recipient_url).bind(&token).bind(LEASE_SECONDS).bind(provider_id).bind(&tracking.number)
        .execute(&mut *tx).await.map_err(crate::error::Error::from)?;
    tx.commit().await.map_err(crate::error::Error::from)?;
    let registration = Registration {
        tracking_number: tracking.number.clone(),
        carrier: tracking.carrier,
        recipient_url: tracking.recipient_url.clone(),
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
                // Preparation can involve multiple bounded lookups. Renew both
                // live claims before the bounded registration request begins.
                let number_owned = claim.renew(store).await
                    .map_err(|_| crate::shipping_api::error::Error::new(provider.id(), crate::shipping_api::error::ErrorKind::Configuration))?;
                let owned = if number_owned {
                    sqlx::query("UPDATE tracking_verifications SET lease_until=unixepoch()+? WHERE provider=? AND tracking_number=? AND lease_token=? AND lease_until>unixepoch()")
                        .bind(LEASE_SECONDS).bind(provider_id).bind(&tracking.number).bind(&token).execute(store.pool()).await
                        .map_err(|_| crate::shipping_api::error::Error::new(provider.id(), crate::shipping_api::error::ErrorKind::Configuration))?.rows_affected() == 1
                } else { false };
                if !owned {
                    return Err(crate::shipping_api::error::Error::new(provider.id(), crate::shipping_api::error::ErrorKind::Throttled { retry_after: 60 }));
                }
                prepared.register().await
            }
        }
    }.await.and_then(|tracker| {
        if tracker.provider != provider.id()
            || !provider.supports_carrier(tracker.carrier)
            || tracker.tracking_code != tracking.number
            || tracking.carrier.is_some_and(|carrier| carrier != tracker.carrier)
            || tracking.recipient_url.as_ref().is_some_and(|url| tracker.public_url.as_ref() != Some(url))
        {
            Err(crate::shipping_api::error::Error::new(provider.id(), crate::shipping_api::error::ErrorKind::InvalidResponse))
        } else {
            Ok(tracker)
        }
    });
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
            if matches!(
                error.kind,
                crate::shipping_api::error::ErrorKind::LocalQueueTimeout
                    | crate::shipping_api::error::ErrorKind::LocalQueueFull
            ) {
                // This operation did not send a provider request at the failed
                // stage. Release only our lease, retaining intent/identity and
                // prior uncertain evidence; do not manufacture provider backoff.
                sqlx::query("UPDATE tracking_verifications SET lease_token=NULL,lease_until=NULL WHERE provider=? AND tracking_number=? AND lease_token=?")
                    .bind(provider_id).bind(&tracking.number).bind(&token).execute(store.pool()).await.map_err(crate::error::Error::from)?;
                let failure = Failure::from_provider(&error);
                slog::warn!(log, "tracking_verification"; "request_id" => request_id, "provider" => provider_id, "outcome" => "not_saved", "error_code" => error.error_code());
                return Err(failure.http(1));
            }
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

#[cfg(test)]
mod tests {
    use crate::{admission, store::Store};

    #[derive(Clone)]
    struct QueueFixture {
        first: std::sync::Arc<std::sync::Mutex<Option<crate::shipping_api::error::ErrorKind>>>,
        calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }

    impl crate::shipping_api::ShippingApi for QueueFixture {
        fn id(&self) -> crate::shipping_api::types::ProviderId {
            crate::shipping_api::types::ProviderId::EasyPost
        }
        fn blocked(&self) -> bool {
            false
        }
        fn for_request(
            &self,
            _: &str,
            _: Option<&str>,
        ) -> std::sync::Arc<dyn crate::shipping_api::ShippingApi> {
            std::sync::Arc::new(self.clone())
        }
        fn check_connection(&self) -> crate::shipping_api::types::ApiFuture<'_, ()> {
            Box::pin(async { Ok(()) })
        }
        fn reconcile_registration<'a>(
            &'a self,
            registration: &'a crate::shipping_api::types::Registration,
        ) -> crate::shipping_api::types::ApiFuture<'a, Option<crate::shipping_api::types::Tracker>>
        {
            Box::pin(async move {
                self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if let Some(kind) = self.first.lock().unwrap().take() {
                    return Err(crate::shipping_api::error::Error::new(self.id(), kind));
                }
                Ok(Some(crate::shipping_api::types::Tracker {
                    id: "trk_existing_fixture".into(),
                    provider: self.id(),
                    tracking_code: registration.tracking_number.clone(),
                    carrier: crate::carrier::Carrier::Ups,
                    status: crate::models::Status::InTransit,
                    public_url: None,
                    carrier_tracking: Default::default(),
                }))
            })
        }
        fn prepare_registration(
            &self,
            _: crate::shipping_api::types::Registration,
        ) -> crate::shipping_api::types::ApiFuture<
            '_,
            Box<dyn crate::shipping_api::PreparedRegistration>,
        > {
            panic!("must reconcile existing tracker, never register twice")
        }
        fn retrieve<'a>(
            &'a self,
            _: &'a str,
            _: &'a str,
        ) -> crate::shipping_api::types::ApiFuture<'a, crate::shipping_api::types::Tracker>
        {
            panic!("not needed by verification")
        }
    }

    #[tokio::test]
    async fn local_queue_failure_releases_lease_without_pin_or_retry_backoff() {
        use crate::shipping_api::error::ErrorKind;
        for kind in [
            ErrorKind::LocalQueueTimeout,
            ErrorKind::LocalQueueFull,
            ErrorKind::Http {
                status: 429,
                retry_after: Some(120),
            },
        ] {
            let upstream = matches!(kind, ErrorKind::Http { .. });
            let dir = tempfile::tempdir().unwrap();
            let store = Store::open(&dir.path().join("queue.sqlite3"))
                .await
                .unwrap();
            let provider = QueueFixture {
                first: std::sync::Arc::new(std::sync::Mutex::new(Some(kind))),
                calls: Default::default(),
            };
            let tracking = crate::models::InputTracking {
                number: "SYNTHETIC-QUEUE-IDENTITY".into(),
                carrier: Some(crate::carrier::Carrier::Ups),
                recipient_url: None,
            };
            let claim = admission::claim_tracking(&store, &tracking.number)
                .await
                .unwrap();
            let log = crate::logging::logger();
            let error = admission::verify(
                &store,
                &claim,
                Some(std::sync::Arc::new(provider.clone())),
                &tracking,
                &log,
                "queue-first",
            )
            .await
            .unwrap_err();
            assert_eq!(error.status_code.as_u16(), 503);
            let state:(i64,i64,Option<String>) = sqlx::query_as("SELECT attempts,next_attempt_at,lease_token FROM tracking_verifications WHERE tracking_number=?")
                .bind(&tracking.number).fetch_one(store.pool()).await.unwrap();
            assert!(state.2.is_none());
            if upstream {
                assert_eq!(state.0, 1);
                let now: i64 = sqlx::query_scalar("SELECT unixepoch()")
                    .fetch_one(store.pool())
                    .await
                    .unwrap();
                assert!(state.1 >= now + 119);
                assert!(
                    admission::verify(
                        &store,
                        &claim,
                        Some(std::sync::Arc::new(provider.clone())),
                        &tracking,
                        &log,
                        "queue-second"
                    )
                    .await
                    .is_err()
                );
                assert_eq!(provider.calls.load(std::sync::atomic::Ordering::SeqCst), 1);
            } else {
                assert_eq!((state.0, state.1), (0, 0));
                let tracker = admission::verify(
                    &store,
                    &claim,
                    Some(std::sync::Arc::new(provider.clone())),
                    &tracking,
                    &log,
                    "queue-second",
                )
                .await
                .unwrap();
                assert_eq!(tracker.id, "trk_existing_fixture");
                let replay = admission::verify(
                    &store,
                    &claim,
                    Some(std::sync::Arc::new(provider.clone())),
                    &tracking,
                    &log,
                    "queue-replay",
                )
                .await
                .unwrap();
                assert_eq!(replay.id, tracker.id);
                assert_eq!(provider.calls.load(std::sync::atomic::Ordering::SeqCst), 2);
            }
            admission::release_tracking(&store, claim).await.unwrap();
            store.close().await.unwrap();
        }
    }

    #[tokio::test]
    async fn same_idempotency_key_succeeds_after_local_queue_failure_without_duplicate_registration()
     {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(&directory.path().join("queue-http.sqlite3"))
            .await
            .unwrap();
        let provider = QueueFixture {
            first: std::sync::Arc::new(std::sync::Mutex::new(Some(
                crate::shipping_api::error::ErrorKind::LocalQueueTimeout,
            ))),
            calls: Default::default(),
        };
        let token = "synthetic-queue-http-owner-credential-at-least32";
        let context = crate::api::Context::new(store.clone(), token, None, None)
            .with_provider(std::sync::Arc::new(provider.clone()));
        let server =
            crate::api::start("127.0.0.1:0".parse().unwrap(), std::sync::Arc::new(context))
                .unwrap();
        let endpoint = format!("http://{}/v1/shipments", server.local_addr());
        let client = reqwest::Client::new();
        let body = serde_json::json!({"direction":"inbound","title":"Synthetic bounded queue parcel","tracking":{"carrier":"UPS","number":"SYNTHETIC-QUEUE-HTTP1"}});
        let response = client
            .post(&endpoint)
            .bearer_auth(token)
            .header("Idempotency-Key", "same-queue-request")
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), 503);
        let error: serde_json::Value = response.json().await.unwrap();
        assert_eq!(error["error_code"], "local_provider_queue_timeout");
        let mut saved_id = None;
        for _ in 0..2 {
            let response = client
                .post(&endpoint)
                .bearer_auth(token)
                .header("Idempotency-Key", "same-queue-request")
                .json(&body)
                .send()
                .await
                .unwrap();
            assert_eq!(response.status().as_u16(), 201);
            let saved: crate::models::Shipment = response.json().await.unwrap();
            if let Some(id) = saved_id {
                assert_eq!(saved.id, id);
            }
            saved_id = Some(saved.id);
        }
        assert_eq!(provider.calls.load(std::sync::atomic::Ordering::SeqCst), 2);
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM shipments")
            .fetch_one(store.pool())
            .await
            .unwrap();
        assert_eq!(count, 1);
        server.close().await.unwrap();
        store.close().await.unwrap();
    }

    #[tokio::test]
    async fn tracking_claims_survive_restart_and_fence_expired_owners() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("claims.sqlite3");
        let store = Store::open(&path).await.unwrap();
        let original = admission::claim_tracking(&store, "TRACKING123456")
            .await
            .unwrap();
        store.close().await.unwrap();
        let store = Store::open(&path).await.unwrap();
        assert!(
            admission::claim_tracking(&store, "TRACKING123456")
                .await
                .is_err()
        );

        sqlx::query("UPDATE admission_tracking SET lease_until=unixepoch()+30")
            .execute(store.pool())
            .await
            .unwrap();
        admission::renew_tracking(&store, &original).await.unwrap();
        let remaining: i64 =
            sqlx::query_scalar("SELECT lease_until-unixepoch() FROM admission_tracking")
                .fetch_one(store.pool())
                .await
                .unwrap();
        assert!((119..=120).contains(&remaining));

        sqlx::query("UPDATE admission_tracking SET lease_until=unixepoch()")
            .execute(store.pool())
            .await
            .unwrap();
        assert!(admission::renew_tracking(&store, &original).await.is_err());
        let successor = admission::claim_tracking(&store, "TRACKING123456")
            .await
            .unwrap();
        assert!(admission::renew_tracking(&store, &original).await.is_err());
        admission::release_tracking(&store, original).await.unwrap();
        assert!(
            admission::claim_tracking(&store, "TRACKING123456")
                .await
                .is_err()
        );
        admission::renew_tracking(&store, &successor).await.unwrap();
        admission::release_tracking(&store, successor)
            .await
            .unwrap();
        let next = admission::claim_tracking(&store, "TRACKING123456")
            .await
            .unwrap();
        admission::release_tracking(&store, next).await.unwrap();
        store.close().await.unwrap();
    }
}
