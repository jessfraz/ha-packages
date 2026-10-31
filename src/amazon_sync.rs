//! Account discovery feeds verified new parcels into the same shipment worker.
//! Existing parcels are polled by ShippingApi::retrieve at the shared cadence.

use std::sync::Arc;

use sqlx::{FromRow, SqlitePool};
use uuid::Uuid;

use crate::{
    amazon::{AmazonClient, DeferredCounts},
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

// Page coverage and parser deferrals are cumulative within the current sweep;
// verified, committed, and Store-refused parcel counts describe this batch.
#[derive(Default)]
struct BatchProgress {
    listing_pages: Option<usize>,
    verified_parcels: usize,
    committed_parcels: usize,
    deferred_store: usize,
    deferred: DeferredCounts,
    complete: Option<bool>,
    has_more: Option<bool>,
    error_code: Option<&'static str>,
}

#[derive(Clone)]
pub struct AmazonDiscoveryWorker {
    pool: SqlitePool,
    store: Store,
    client: Arc<AmazonClient>,
    log: slog::Logger,
}

impl AmazonDiscoveryWorker {
    pub fn new(store: Store, client: Arc<AmazonClient>) -> Self {
        Self {
            pool: store.pool().clone(),
            store,
            client,
            log: crate::logging::logger(),
        }
    }

    pub fn with_logger(mut self, log: slog::Logger) -> Self {
        self.log = log;
        self
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
        let result = self.client.discovery_batch().await;
        let mut progress = BatchProgress::default();
        let (delay, succeeded) = match result {
            Ok(mut batch) => {
                progress = BatchProgress {
                    listing_pages: Some(batch.listing_pages),
                    verified_parcels: batch.parcels.len(),
                    deferred: batch.deferred_reasons.clone(),
                    complete: Some(batch.complete),
                    has_more: Some(batch.has_more),
                    ..Default::default()
                };
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
                            ) => progress.deferred_store += 1,
                            Err(error) => return Err(error),
                        }
                    }
                    drop(guard);
                    progress.committed_parcels = imported.len();
                    self.client.remember_imported(&imported).await;
                } else if let Err(error) = guard
                    && batch.failure.is_none()
                {
                    batch.failure = Some(crate::amazon::connection::ConnectionError::new(
                        crate::amazon::connection::ConnectionStage::Account,
                        error,
                    ));
                }
                if let Some(error) = batch.failure {
                    progress.error_code = Some(error.reason.as_str());
                    (
                        retry_delay(
                            claim.attempts.saturating_add(1),
                            error.retry_after().unwrap_or(0),
                        ),
                        false,
                    )
                } else {
                    (
                        if batch.has_more {
                            60
                        } else {
                            discovery_delay("amazon_account")
                        },
                        true,
                    )
                }
            }
            Err(error) => {
                progress.error_code = Some(error.reason.as_str());
                (
                    retry_delay(
                        claim.attempts.saturating_add(1),
                        error.retry_after().unwrap_or(0),
                    ),
                    false,
                )
            }
        };
        let scheduled = sqlx::query("UPDATE amazon_discovery SET next_attempt_at=unixepoch()+?,attempts=CASE WHEN ? THEN 0 ELSE attempts+1 END,lease_token=NULL,lease_until=NULL WHERE id=1 AND lease_token=? AND lease_until>unixepoch()")
            .bind(delay).bind(succeeded).bind(&claim.lease_token)
            .execute(&self.pool).await?;
        if scheduled.rows_affected() == 1 {
            // complete describes page coverage; Store refusals must never be
            // represented as a successful import, even after the final page.
            let outcome = if !succeeded {
                "error"
            } else if progress.deferred_store > 0 {
                "not_saved"
            } else {
                "saved"
            };
            slog::info!(self.log, "amazon_discovery_batch";
                "provider" => "amazon_account", "operation" => "discovery", "outcome" => outcome,
                "listing_pages" => progress.listing_pages,
                "verified_parcels" => progress.verified_parcels,
                "committed_parcels" => progress.committed_parcels,
                "deferred_store" => progress.deferred_store,
                "deferred_link" => progress.deferred.unsupported_link,
                "deferred_identity" => progress.deferred.missing_identity,
                "deferred_product" => progress.deferred.missing_product,
                "deferred_tracking" => progress.deferred.unverified_tracking,
                "deferred_limit" => progress.deferred.request_limit,
                "complete" => progress.complete, "has_more" => progress.has_more,
                "next_delay_seconds" => delay, "error_code" => progress.error_code,
            );
        }
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::AmazonDiscoveryWorker;
    use crate::{
        amazon::AmazonClient,
        carrier::Carrier,
        models::{CreateShipment, Direction, InputTracking},
        store::Store,
    };
    use serde::Deserialize;
    use sha2::{Digest, Sha256};
    use std::{
        io::Write,
        os::unix::fs::OpenOptionsExt,
        sync::{
            Arc, Mutex,
            atomic::{AtomicBool, Ordering},
        },
    };
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[derive(Clone, Default)]
    struct Capture(Arc<Mutex<Vec<u8>>>);

    impl Write for Capture {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[derive(Debug, Deserialize)]
    struct BatchLog {
        level: String,
        outcome: String,
        listing_pages: usize,
        verified_parcels: usize,
        committed_parcels: usize,
        deferred_store: usize,
        deferred_link: usize,
        deferred_identity: usize,
        deferred_product: usize,
        deferred_tracking: usize,
        deferred_limit: usize,
        complete: bool,
        has_more: bool,
        next_delay_seconds: i64,
        order_fingerprint: Option<String>,
        error_code: Option<String>,
    }

    #[derive(Debug, Deserialize)]
    #[serde(tag = "event")]
    enum Diagnostic {
        #[serde(rename = "amazon_discovery_batch")]
        Batch(BatchLog),
        #[serde(rename = "amazon_connection")]
        Deferred { order_fingerprint: String },
    }

    fn tracking_path(package: usize) -> String {
        format!(
            "/progress-tracker/package?orderId=111-0000000-{package:07}&shipmentId=SHIP{package}&packageIndex=0"
        )
    }

    #[tokio::test]
    async fn batch_logs_prove_fenced_page_coverage_and_distinguish_store_refusals() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("registry.sqlite3"))
            .await
            .unwrap();
        let existing = store
            .create(
                CreateShipment {
                    title: "PRIVATE_OWNER_TITLE".into(),
                    direction: Direction::Outbound,
                    merchant: None,
                    order_reference: None,
                    signature_required: None,
                    tracking: InputTracking {
                        number: "9400000000000000000007".into(),
                        carrier: Some(Carrier::Usps),
                        recipient_url: None,
                    },
                    source: None,
                },
                "owner",
                "private-existing",
                None,
            )
            .await
            .unwrap();
        let capture = Capture::default();
        let log = crate::logging::with_writer(capture.clone()).new(slog::o!(
            "lease_token" => "PRIVATE_LEASE_SENTINEL",
            "order_fingerprint" => "PRIVATE_INVALID_FINGERPRINT",
            "url" => "https://www.amazon.com/PRIVATE_URL_SENTINEL",
            "raw_html" => "PRIVATE_HTML_SENTINEL",
            "refresh_token" => "PRIVATE_REFRESH_SENTINEL",
        ));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint =
            reqwest::Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
        let invalidate = Arc::new(AtomicBool::new(false));
        let invalidation = invalidate.clone();
        let unavailable = Arc::new(AtomicBool::new(false));
        let outage = unavailable.clone();
        let upstream_store = store.clone();
        let upstream = tokio::spawn(async move {
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                let mut buffer = [0; 4096];
                let path = loop {
                    let read = socket.read(&mut buffer).await.unwrap();
                    assert!(read > 0);
                    request.extend_from_slice(&buffer[..read]);
                    if let Some(end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
                        let headers = std::str::from_utf8(&request[..end]).unwrap();
                        let length = headers
                            .lines()
                            .find_map(|line| {
                                line.to_ascii_lowercase()
                                    .strip_prefix("content-length: ")
                                    .map(str::to_owned)
                            })
                            .map(|length| length.parse::<usize>().unwrap())
                            .unwrap_or(0);
                        if request.len() >= end + 4 + length {
                            break headers
                                .lines()
                                .next()
                                .unwrap()
                                .split_whitespace()
                                .nth(1)
                                .unwrap()
                                .to_owned();
                        }
                    }
                };
                let mut status = 200;
                let body = match path.as_str() {
                    "/auth/token" => r#"{"access_token":"PRIVATE_ACCESS_SENTINEL"}"#.into(),
                    "/ap/exchangetoken/cookies" => r#"{"response":{"tokens":{"cookies":{".amazon.com":[{"Name":"session-id","Value":"PRIVATE_COOKIE_SENTINEL","Path":"/"}]}}}}"#.into(),
                    path if path.starts_with("/your-orders/orders?") && outage.swap(false, Ordering::AcqRel) => {
                        status = 503;
                        "Service unavailable".into()
                    }
                    path if path.starts_with("/your-orders/orders?") => {
                        if path == "/your-orders/orders?timeFilter=months-3" && invalidation.swap(false, Ordering::AcqRel) {
                            sqlx::query("UPDATE amazon_discovery SET lease_token='PRIVATE_LEASE_SENTINEL' WHERE id=1").execute(upstream_store.pool()).await.unwrap();
                        }
                        let page = reqwest::Url::parse(&format!("https://www.amazon.com{path}")).unwrap().query_pairs().find(|(key, _)| key == "startIndex").map(|(_, value)| value.parse::<usize>().unwrap() / 10 + 1).unwrap_or(1);
                        let mut body = format!(r#"<a class="item-card__link" href="{}" aria-label="PRIVATE_PRODUCT_{page}, Order placed 1 October 2026, Shipped"></a>"#, tracking_path(page));
                        if page == 1 {
                            body.push_str(&format!(r#"<a class="item-card__link" href="{}" aria-label="PRIVATE_DEFERRED_PRODUCT, Order placed 1 October 2026, Shipped"></a>"#, tracking_path(8)));
                        }
                        if page < 7 {
                            body.push_str(&format!(r#"<script type="a-state" data-a-state='{{"key":"chunkState"}}'>{{"nextStartIndex":{},"timeFilter":"months-3"}}</script>"#, page * 10));
                        }
                        body
                    }
                    path => {
                        let package = (1..=8).find(|package| tracking_path(*package) == path).expect("unexpected fixture request");
                        format!(r#"{}<script id="page-state">{{"trackingId":"940000000000000000000{package}","shortStatus":"IN_TRANSIT"}}</script>"#, if package == 8 { "" } else { "<h3>Delivery By USPS</h3>" })
                    }
                };
                let response = format!(
                    "HTTP/1.1 {status} fixture\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                socket.write_all(response.as_bytes()).await.unwrap();
            }
        });
        let credential = dir.path().join("account.json");
        std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&credential).unwrap()
            .write_all(br#"{"refresh_token":"PRIVATE_REFRESH_SENTINEL","device_serial":"PRIVATE_DEVICE_SENTINEL","customer_id":"PRIVATE_CUSTOMER_SENTINEL","generation":1,"imports_enabled":true}"#).unwrap();
        let client = Arc::new(
            AmazonClient::for_test(credential, endpoint)
                .unwrap()
                .with_logger(log.clone()),
        );
        let worker = AmazonDiscoveryWorker::new(store.clone(), client).with_logger(log);
        for _ in 0..3 {
            sqlx::query("UPDATE amazon_discovery SET next_attempt_at=unixepoch()-1 WHERE id=1")
                .execute(store.pool())
                .await
                .unwrap();
            assert!(worker.tick().await.unwrap());
        }
        unavailable.store(true, Ordering::Release);
        sqlx::query("UPDATE amazon_discovery SET next_attempt_at=unixepoch()-1 WHERE id=1")
            .execute(store.pool())
            .await
            .unwrap();
        assert!(worker.tick().await.unwrap());
        tokio::time::pause();
        tokio::time::advance(std::time::Duration::from_secs(331)).await;
        tokio::time::resume();
        // Losing the lease during the next read must not publish completion.
        invalidate.store(true, Ordering::Release);
        sqlx::query("UPDATE amazon_discovery SET next_attempt_at=unixepoch()-1 WHERE id=1")
            .execute(store.pool())
            .await
            .unwrap();
        assert!(worker.tick().await.unwrap());
        let text = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
        let entries: Vec<Diagnostic> = text
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let batches: Vec<_> = entries
            .iter()
            .filter_map(|entry| {
                if let Diagnostic::Batch(batch) = entry {
                    Some(batch)
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(batches.len(), 4);
        for (batch, (pages, verified, committed, complete)) in
            batches
                .iter()
                .zip([(3, 3, 3, false), (6, 3, 3, false), (7, 1, 0, true)])
        {
            assert_eq!(batch.listing_pages, pages);
            assert_eq!(batch.verified_parcels, verified);
            assert_eq!(batch.committed_parcels, committed);
            assert_eq!(batch.deferred_store, usize::from(complete));
            assert_eq!(batch.complete, complete);
            assert_eq!(batch.has_more, !complete);
            assert_eq!(
                batch.deferred_link
                    + batch.deferred_identity
                    + batch.deferred_product
                    + batch.deferred_limit,
                0
            );
            assert_eq!(batch.deferred_tracking, 1);
            assert!(batch.order_fingerprint.is_none());
            assert_eq!(batch.outcome, if complete { "not_saved" } else { "saved" });
            assert_eq!(batch.level, if complete { "warn" } else { "info" });
            assert!(batch.error_code.is_none());
            if complete {
                assert!((1800..=1980).contains(&batch.next_delay_seconds));
            } else {
                assert_eq!(batch.next_delay_seconds, 60);
            }
        }
        let failed = batches[3];
        assert_eq!(failed.outcome, "error");
        assert_eq!(failed.level, "warn");
        assert_eq!(failed.error_code.as_deref(), Some("upstream_http"));
        assert_eq!(failed.listing_pages, 0);
        assert_eq!(failed.verified_parcels + failed.committed_parcels, 0);
        assert!(!failed.complete && failed.has_more);
        assert!((300..=330).contains(&failed.next_delay_seconds));
        let order_fingerprint = Sha256::digest(b"111-0000000-0000008")
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let deferred: Vec<_> = entries
            .iter()
            .filter_map(|entry| {
                if let Diagnostic::Deferred { order_fingerprint } = entry {
                    Some(order_fingerprint)
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(deferred, [&order_fingerprint, &order_fingerprint]);
        assert!(!text.contains("PRIVATE_"));
        assert!(!text.contains("111-0000000"));
        assert!(!text.contains("940000000000"));
        assert_eq!(store.get(existing.id).await.unwrap().title, existing.title);
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM shipments")
                .fetch_one(store.pool())
                .await
                .unwrap(),
            7
        );
        upstream.abort();
        assert!(upstream.await.unwrap_err().is_cancelled());
        store.close().await.unwrap();
    }

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
