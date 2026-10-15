//! Durable provider-neutral tracking work. Network calls never hold a database
//! transaction; generation/token fences reject stale results.

use crate::{
    error::Error,
    models::{CarrierTracking, EventKind, Status},
    shipping_api::{
        ShippingApi,
        error::{Error as SyncError, ErrorKind},
        types::{Registration, Tracker},
    },
};
use sha2::{Digest, Sha256};
use sqlx::{FromRow, SqlitePool};
use std::sync::Arc;
use uuid::Uuid;

const LEASE_SECONDS: i64 = 60;
const MAX_RETRY_SECONDS: i64 = 6 * 60 * 60;

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
    carrier_tracking: String,
}

#[derive(Clone)]
pub struct SyncWorker {
    pool: SqlitePool,
    client: Arc<dyn ShippingApi>,
}

impl SyncWorker {
    pub fn new(pool: SqlitePool, client: Arc<dyn ShippingApi>) -> Self {
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
                 WHERE archived = 0 AND tracking_number IS NOT NULL AND tracking_provider = ?
                   AND registration_state != 'rejected'
                   AND next_attempt_at <= unixepoch()
                   AND (lease_until IS NULL OR lease_until <= unixepoch())
                 ORDER BY next_attempt_at, id LIMIT 1
             )
             RETURNING id, tracking_generation, tracking_number, tracking_carrier,
                       tracker_id, lease_token",
        )
        .bind(Uuid::now_v7().to_string())
        .bind(LEASE_SECONDS)
        .bind(self.client.id().as_str())
        .fetch_optional(&self.pool)
        .await?;
        let Some(claim) = claim else { return Ok(false) };

        let client = self.client.for_request(&claim.lease_token, Some(&claim.id));
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
                // Commit registration intent before any provider request. Even
                // the first attempt must find an earlier, uncommitted tracker.
                let started = sqlx::query_scalar::<_, i64>(
                    "UPDATE shipments SET registration_started_at=coalesce(registration_started_at,unixepoch())
                     WHERE id=? AND tracking_generation=? AND lease_token=?
                       AND lease_until>unixepoch() AND archived=0 AND next_attempt_at IS NOT NULL AND tracking_provider=?
                     RETURNING registration_started_at",
                ).bind(&claim.id).bind(claim.tracking_generation).bind(&claim.lease_token).bind(self.client.id().as_str())
                    .fetch_optional(&self.pool).await?;
                let Some(started) = started else {
                    self.release_cancelled_registration(&claim).await?;
                    return Ok(true);
                };
                let registration = Registration {
                    tracking_number: claim.tracking_number.clone(),
                    carrier,
                    started_at: started,
                };
                match client.reconcile_registration(&registration).await {
                    Ok(Some(tracker)) => Ok(tracker),
                    Ok(None) => {
                        // Wait for the outbound slot before checking the
                        // generation fence; cancellation during pacing must
                        // not start a paid registration for an obsolete claim.
                        let registration = match client.prepare_registration(registration).await {
                            Ok(slot) => slot,
                            Err(error) => {
                                self.complete(&claim, Err(error)).await?;
                                return Ok(true);
                            }
                        };
                        // The lookup crossed a network boundary. Do not create
                        // tracking for a record that was edited or archived meanwhile.
                        let owned = sqlx::query_scalar::<_, bool>(
                                "SELECT EXISTS(SELECT 1 FROM shipments WHERE id=? AND tracking_generation=? AND lease_token=? AND lease_until>unixepoch() AND archived=0 AND next_attempt_at IS NOT NULL AND tracking_provider=?)",
                            ).bind(&claim.id).bind(claim.tracking_generation).bind(&claim.lease_token).bind(self.client.id().as_str())
                                .fetch_one(&self.pool).await?;
                        if !owned {
                            drop(registration);
                            self.release_cancelled_registration(&claim).await?;
                            return Ok(true);
                        }
                        registration.register().await
                    }
                    Err(error) => Err(error),
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
        sqlx::query("UPDATE shipments SET lease_token=NULL,lease_until=NULL WHERE id=? AND tracking_generation=? AND lease_token=? AND lease_until>unixepoch() AND archived=0 AND next_attempt_at IS NULL AND tracking_provider=?")
            .bind(&claim.id).bind(claim.tracking_generation).bind(&claim.lease_token).bind(self.client.id().as_str())
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
               AND lease_until > unixepoch() AND archived = 0 AND tracking_provider = ?
             RETURNING carrier_status, tracking_carrier, override_status, attempts, carrier_tracking,
                       max(0, unixepoch() - coalesce(unixepoch(status_since), unixepoch())) AS status_age",
        )
        .bind(&claim.id)
        .bind(claim.tracking_generation)
        .bind(&claim.lease_token)
        .bind(self.client.id().as_str())
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
                    sqlx::query("INSERT INTO events (shipment_id, kind, actor_id, detail, snapshot) VALUES (?, ?, ?, ?, ?)")
                        .bind(&claim.id)
                        .bind(serde_json::to_string(&EventKind::CarrierUpdated)?)
                        .bind(self.client.id().as_str())
                        .bind(detail)
                        .bind(serde_json::to_string(&snapshot)?)
                        .execute(&mut *transaction)
                        .await?;
                }
            }
            Err(SyncError {
                kind: ErrorKind::Throttled { retry_after },
                ..
            }) => {
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
    use crate::shipping_api::{
        easypost::EasyPostClient,
        error::{Error as SyncError, ErrorKind},
        types::ProviderId,
    };
    use crate::{
        models::{CreateShipment, SetOverride, Status, Tracking},
        store::Store,
        sync::{Claim, SyncWorker, poll_delay},
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
            std::sync::Arc::new(EasyPostClient::new("unused-key").unwrap()),
        );
        worker
            .complete(
                &claim,
                Err(SyncError::new(
                    ProviderId::EasyPost,
                    ErrorKind::Throttled { retry_after: 1 },
                )),
            )
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
            .complete(
                &claim,
                Err(SyncError::new(
                    ProviderId::EasyPost,
                    ErrorKind::Throttled { retry_after: 1 },
                )),
            )
            .await
            .unwrap();
        let deferred = store.get(shipment.id).await.unwrap();
        assert!(deferred.sync.next_attempt_at.is_some());
        assert_eq!(deferred.sync.attempts, 0);
        assert!(deferred.sync.last_error.is_none());
    }
}
