//! Account discovery feeds verified new parcels into the same shipment worker.
//! Existing parcels are polled by ShippingApi::retrieve at the shared cadence.

use std::sync::Arc;

use sqlx::{FromRow, SqlitePool};
use uuid::Uuid;

use crate::{
    amazon::AmazonClient,
    error::Error,
    shipping_api::ShippingApi,
    store::Store,
    sync::{discovery_delay, retry_delay},
};

#[derive(FromRow)]
struct DiscoveryClaim {
    lease_token: String,
    attempts: i64,
}

#[derive(Clone)]
pub struct AmazonDiscoveryWorker {
    pool: SqlitePool,
    store: Store,
    client: Arc<AmazonClient>,
}

impl AmazonDiscoveryWorker {
    pub fn new(store: Store, client: Arc<AmazonClient>) -> Self {
        Self {
            pool: store.pool().clone(),
            store,
            client,
        }
    }

    pub async fn queue(store: &Store) -> Result<(), Error> {
        sqlx::query("UPDATE amazon_discovery SET next_attempt_at=CASE WHEN attempts>0 AND next_attempt_at>unixepoch() THEN next_attempt_at ELSE min(next_attempt_at,max(unixepoch(),coalesce(last_attempt_at+60,unixepoch()))) END WHERE id=1 AND (lease_until IS NULL OR lease_until<=unixepoch())")
            .execute(store.pool()).await?;
        Ok(())
    }

    pub async fn enabled(store: &Store) -> Result<(), Error> {
        sqlx::query("UPDATE amazon_discovery SET next_attempt_at=unixepoch(),attempts=0,lease_token=NULL,lease_until=NULL WHERE id=1")
            .execute(store.pool()).await?;
        Ok(())
    }

    pub async fn tick(&self) -> Result<bool, Error> {
        if !self.client.status().await.imports_enabled || self.client.blocked() {
            return Ok(false);
        }
        let claim = sqlx::query_as::<_, DiscoveryClaim>(
            "UPDATE amazon_discovery SET lease_token=?,lease_until=unixepoch()+600,last_attempt_at=unixepoch() WHERE id=1 AND next_attempt_at<=unixepoch() AND (lease_until IS NULL OR lease_until<=unixepoch()) RETURNING lease_token,attempts",
        ).bind(Uuid::now_v7().to_string()).fetch_optional(&self.pool).await?;
        let Some(claim) = claim else {
            return Ok(false);
        };
        let result = self.client.discover().await;
        let (delay, succeeded) = match result {
            Ok(batch) => {
                // An old response cannot import after disconnect or account replacement.
                let guard = self.client.commit_guard(batch.generation).await;
                let mut imported = Vec::new();
                if let Ok(guard) = guard {
                    for parcel in &batch.parcels {
                        match self
                            .store
                            .import_amazon(parcel, Some(&claim.lease_token))
                            .await
                        {
                            Ok(_) => imported.push(parcel.source_key.clone()),
                            // Identity conflicts stay deferred rather than modifying a record.
                            Err(
                                Error::Conflict(_)
                                | Error::TrackingDuplicate { .. }
                                | Error::Invalid(_)
                                | Error::TrackingInvalid
                                | Error::TrackingRequired
                                | Error::CarrierRequired,
                            ) => {}
                            Err(error) => return Err(error),
                        }
                    }
                    drop(guard);
                    self.client.remember_imported(&imported).await;
                }
                (discovery_delay("amazon_account"), true)
            }
            Err(error) => (
                retry_delay(
                    claim.attempts.saturating_add(1),
                    error.retry_after().unwrap_or(0),
                ),
                false,
            ),
        };
        sqlx::query("UPDATE amazon_discovery SET next_attempt_at=unixepoch()+?,attempts=CASE WHEN ? THEN 0 ELSE attempts+1 END,lease_token=NULL,lease_until=NULL WHERE id=1 AND lease_token=? AND lease_until>unixepoch()")
            .bind(delay).bind(succeeded).bind(&claim.lease_token)
            .execute(&self.pool).await?;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::AmazonDiscoveryWorker;
    use crate::store::Store;

    #[tokio::test]
    async fn refresh_coalesces_respects_backoff_and_does_not_replace_active_discovery() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("registry.sqlite3"))
            .await
            .unwrap();
        sqlx::query("UPDATE amazon_discovery SET attempts=3,last_attempt_at=unixepoch(),next_attempt_at=unixepoch()+600 WHERE id=1").execute(store.pool()).await.unwrap();
        let before: i64 =
            sqlx::query_scalar("SELECT next_attempt_at FROM amazon_discovery WHERE id=1")
                .fetch_one(store.pool())
                .await
                .unwrap();
        AmazonDiscoveryWorker::queue(&store).await.unwrap();
        let after: i64 =
            sqlx::query_scalar("SELECT next_attempt_at FROM amazon_discovery WHERE id=1")
                .fetch_one(store.pool())
                .await
                .unwrap();
        assert_eq!(before, after);
        sqlx::query("UPDATE amazon_discovery SET attempts=0,last_attempt_at=unixepoch(),next_attempt_at=unixepoch()+1800 WHERE id=1").execute(store.pool()).await.unwrap();
        AmazonDiscoveryWorker::queue(&store).await.unwrap();
        let due: i64 = sqlx::query_scalar(
            "SELECT next_attempt_at-unixepoch() FROM amazon_discovery WHERE id=1",
        )
        .fetch_one(store.pool())
        .await
        .unwrap();
        assert!((59..=60).contains(&due));
        sqlx::query("UPDATE amazon_discovery SET lease_token='active',lease_until=unixepoch()+600 WHERE id=1").execute(store.pool()).await.unwrap();
        AmazonDiscoveryWorker::queue(&store).await.unwrap();
        let token: String =
            sqlx::query_scalar("SELECT lease_token FROM amazon_discovery WHERE id=1")
                .fetch_one(store.pool())
                .await
                .unwrap();
        assert_eq!(token, "active");
    }
}
