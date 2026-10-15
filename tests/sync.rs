use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use shipments::models::{
    CreateShipment, EventKind, Patch, SetOverride, Shipment, Status, Tracking, UpdateShipment,
};
use shipments::store::Store;
use shipments::sync::{EasyPostClient, SyncError, SyncWorker, TrackerRejection};
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

struct Fixture {
    _directory: tempfile::TempDir,
    path: PathBuf,
    store: Store,
}

impl Fixture {
    async fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("shipments.sqlite3");
        let store = Store::open(&path).await.unwrap();
        Self {
            _directory: directory,
            path,
            store,
        }
    }

    async fn shipment(&self, number: &str) -> Shipment {
        self.store
            .create(
                CreateShipment {
                    title: "Replacement cable".into(),
                    merchant: None,
                    order_reference: None,
                    tracking: Tracking {
                        carrier: Some("USPS".into()),
                        number: number.into(),
                    },
                    source: None,
                },
                "test",
                number,
                None,
            )
            .await
            .unwrap()
    }
}

struct Incoming {
    method: String,
    path: String,
    authorization: Option<String>,
    body: Vec<u8>,
    response: mpsc::Sender<Reply>,
}

impl Incoming {
    fn reply(self, reply: Reply) {
        self.response.send(reply).unwrap();
    }

    fn tracker(self, number: &str, status: Status) {
        self.reply(Reply::tracker(number, status));
    }
}

struct Reply {
    status: u16,
    headers: Vec<(String, String)>,
    body: String,
}

#[derive(Serialize)]
struct TrackerReply<'a> {
    id: &'a str,
    tracking_code: &'a str,
    carrier: &'a str,
    status: Status,
    #[serde(skip_serializing_if = "Option::is_none")]
    public_url: Option<&'a str>,
}

impl Reply {
    fn tracker(number: &str, status: Status) -> Self {
        Self {
            status: 200,
            headers: Vec::new(),
            body: serde_json::to_string(&TrackerReply {
                id: "trk_test",
                tracking_code: number,
                carrier: "USPS",
                status,
                public_url: None,
            })
            .unwrap(),
        }
    }

    fn error(status: u16) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body: "provider echoed test-secret and private request data".into(),
        }
    }
}

// A real HTTP server with per-request response gates. Tests can change durable
// registry state while a provider request is in flight, without mocking HTTP.
struct Provider {
    base: String,
    incoming: UnboundedReceiver<Incoming>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Provider {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}/v2/", listener.local_addr().unwrap());
        listener.set_nonblocking(true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = Arc::clone(&stop);
        let (sender, incoming) = unbounded_channel();
        let thread = thread::spawn(move || {
            let mut handlers = Vec::new();
            while !stopping.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let sender = sender.clone();
                        let stopping = Arc::clone(&stopping);
                        handlers.push(thread::spawn(move || {
                            let Some((mut stream, method, path, authorization, body)) = read_request(stream) else { return };
                            let (response, receiver) = mpsc::channel();
                            if sender.send(Incoming { method, path, authorization, body, response }).is_err() { return; }
                            while !stopping.load(Ordering::Relaxed) {
                                match receiver.recv_timeout(Duration::from_millis(20)) {
                                    Ok(reply) => {
                                        let chunked = reply.headers.iter().any(|(key, value)| key.eq_ignore_ascii_case("transfer-encoding") && value == "chunked");
                                        let headers = reply.headers.iter().map(|(key, value)| format!("{key}: {value}\r\n")).collect::<String>();
                                        let length = if chunked { String::new() } else { format!("Content-Length: {}\r\n", reply.body.len()) };
                                        let head = format!("HTTP/1.1 {} Test\r\nContent-Type: application/json\r\n{length}Connection: close\r\n{headers}\r\n", reply.status);
                                        if stream.write_all(head.as_bytes()).is_ok() {
                                            if chunked {
                                                let _ = write!(stream, "{:x}\r\n{}\r\n0\r\n\r\n", reply.body.len(), reply.body);
                                            } else {
                                                let _ = stream.write_all(reply.body.as_bytes());
                                            }
                                        }
                                        break;
                                    }
                                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                                }
                            }
                        }));
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2))
                    }
                    Err(_) => break,
                }
            }
            for handler in handlers {
                handler.join().unwrap();
            }
        });
        Self {
            base,
            incoming,
            stop,
            thread: Some(thread),
        }
    }

    fn client(&self) -> EasyPostClient {
        EasyPostClient::for_loopback("test-secret", &self.base).unwrap()
    }

    fn worker(&self, store: &Store) -> SyncWorker {
        SyncWorker::new(store.pool().clone(), self.client())
    }

    async fn request(&mut self) -> Incoming {
        tokio::time::timeout(Duration::from_secs(3), self.incoming.recv())
            .await
            .unwrap()
            .unwrap()
    }
}

impl Drop for Provider {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            thread.join().unwrap();
        }
    }
}

type ParsedRequest = (TcpStream, String, String, Option<String>, Vec<u8>);

fn read_request(mut stream: TcpStream) -> Option<ParsedRequest> {
    // macOS can inherit nonblocking mode from the listener. Each connection
    // has its own thread and must wait for the actual HTTP bytes.
    stream.set_nonblocking(false).ok()?;
    stream
        .set_read_timeout(Some(Duration::from_millis(500)))
        .ok()?;
    let mut header = Vec::new();
    while !header.ends_with(b"\r\n\r\n") {
        let mut byte = [0_u8];
        stream.read_exact(&mut byte).ok()?;
        header.push(byte[0]);
        if header.len() > 16 * 1024 {
            return None;
        }
    }
    let header = String::from_utf8(header).ok()?;
    let mut lines = header.lines();
    let mut request = lines.next()?.split_whitespace();
    let method = request.next()?.to_owned();
    let path = request.next()?.to_owned();
    let mut length = 0;
    let mut authorization = None;
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            if name.eq_ignore_ascii_case("content-length") {
                length = value.trim().parse::<usize>().ok()?;
            }
            if name.eq_ignore_ascii_case("authorization") {
                authorization = Some(value.trim().to_owned());
            }
        }
    }
    if length > 16 * 1024 {
        return None;
    }
    let mut body = vec![0_u8; length];
    stream.read_exact(&mut body).ok()?;
    Some((stream, method, path, authorization, body))
}

fn start_tick(
    worker: &SyncWorker,
) -> tokio::task::JoinHandle<Result<bool, shipments::error::Error>> {
    let worker = worker.clone();
    tokio::spawn(async move { worker.tick().await })
}

async fn due(store: &Store, id: uuid::Uuid) {
    // Advance the persisted scheduling boundary instead of sleeping 30 minutes.
    sqlx::query("UPDATE shipments SET next_attempt_at = unixepoch() WHERE id = ?")
        .bind(id.to_string())
        .execute(store.pool())
        .await
        .unwrap();
}

async fn delay(store: &Store, id: uuid::Uuid) -> Option<i64> {
    sqlx::query_scalar("SELECT next_attempt_at - unixepoch() FROM shipments WHERE id = ?")
        .bind(id.to_string())
        .fetch_one(store.pool())
        .await
        .unwrap()
}

async fn observe(
    provider: &mut Provider,
    worker: &SyncWorker,
    number: &str,
    status: Status,
) -> IncomingDetails {
    let task = start_tick(worker);
    let request = provider.request().await;
    let details = IncomingDetails {
        method: request.method.clone(),
        path: request.path.clone(),
    };
    request.tracker(number, status);
    assert!(task.await.unwrap().unwrap());
    details
}

struct IncomingDetails {
    method: String,
    path: String,
}

#[tokio::test]
async fn registers_retrieves_and_only_revises_changed_observations() {
    #[derive(Deserialize)]
    struct Registration {
        tracker: TrackingRequest,
    }
    #[derive(Deserialize)]
    struct TrackingRequest {
        tracking_code: String,
        carrier: String,
    }

    let fixture = Fixture::new().await;
    let shipment = fixture.shipment("TRACK1").await;
    let mut provider = Provider::start();
    let worker = provider.worker(&fixture.store);
    let task = start_tick(&worker);
    let request = provider.request().await;
    assert_eq!(request.method, "POST");
    assert_eq!(request.path, "/v2/trackers");
    assert_eq!(
        request.authorization.as_deref(),
        Some("Basic dGVzdC1zZWNyZXQ6")
    );
    let registration: Registration = serde_json::from_slice(&request.body).unwrap();
    assert_eq!(registration.tracker.tracking_code, "TRACK1");
    assert_eq!(registration.tracker.carrier, "usps");
    request.tracker("TRACK1", Status::InTransit);
    assert!(task.await.unwrap().unwrap());
    let first = fixture.store.get(shipment.id).await.unwrap();
    assert_eq!(first.status, Status::InTransit);
    assert_eq!(first.revision, shipment.revision + 1);
    assert_eq!(first.sync.tracker_id.as_deref(), Some("trk_test"));
    assert!((1799..=1800).contains(&delay(&fixture.store, shipment.id).await.unwrap()));
    due(&fixture.store, shipment.id).await;
    let request = observe(&mut provider, &worker, "TRACK1", Status::InTransit).await;
    assert_eq!(request.method, "GET");
    assert_eq!(request.path, "/v2/trackers/trk_test");
    let repeated = fixture.store.get(shipment.id).await.unwrap();
    assert_eq!(repeated.revision, first.revision);
    assert_eq!(repeated.updated_at, first.updated_at);
    let events = fixture.store.events(shipment.id, 0, 100).await.unwrap();
    let observations: Vec<_> = events
        .iter()
        .filter(|event| matches!(event.kind, EventKind::CarrierUpdated))
        .collect();
    assert_eq!(observations.len(), 1);
    assert_eq!(observations[0].actor_id, "easypost");
    let snapshot = observations[0].snapshot.as_ref().unwrap();
    assert_eq!(snapshot.status, Status::InTransit);
    assert_eq!(snapshot.revision, first.revision);

    due(&fixture.store, shipment.id).await;
    observe(&mut provider, &worker, "TRACK1", Status::OutForDelivery).await;
    assert!((299..=300).contains(&delay(&fixture.store, shipment.id).await.unwrap()));
    due(&fixture.store, shipment.id).await;
    observe(&mut provider, &worker, "TRACK1", Status::Delivered).await;
    assert_eq!(delay(&fixture.store, shipment.id).await, None);
    assert!(!worker.tick().await.unwrap());
}

#[tokio::test]
async fn worker_persists_canonical_carrier_and_safe_url_without_revising_identical_status() {
    let fixture = Fixture::new().await;
    let shipment = fixture.shipment("TRACK-URL").await;
    let mut provider = Provider::start();
    let worker = provider.worker(&fixture.store);
    let task = start_tick(&worker);
    provider.request().await.reply(Reply {
        status: 200,
        headers: Vec::new(),
        body: serde_json::to_string(&TrackerReply {
            id: "trk_test",
            tracking_code: "TRACK-URL",
            carrier: "uSpS",
            status: Status::Unknown,
            public_url: Some("https://track.easypost.com/public-tracker"),
        })
        .unwrap(),
    });
    assert!(task.await.unwrap().unwrap());
    let first = fixture.store.get(shipment.id).await.unwrap();
    assert_eq!(first.tracking.carrier.as_deref(), Some("usps"));
    assert_eq!(
        first.sync.tracking_url.as_deref(),
        Some("https://track.easypost.com/public-tracker")
    );
    due(&fixture.store, shipment.id).await;
    let task = start_tick(&worker);
    provider.request().await.reply(Reply {
        status: 200,
        headers: Vec::new(),
        body: serde_json::to_string(&TrackerReply {
            id: "trk_test",
            tracking_code: "TRACK-URL",
            carrier: "USPS",
            status: Status::Unknown,
            public_url: Some("https://private-data.example.invalid/tracker"),
        })
        .unwrap(),
    });
    assert!(task.await.unwrap().unwrap());
    let refreshed = fixture.store.get(shipment.id).await.unwrap();
    assert!(refreshed.sync.tracking_url.is_none());
    assert_eq!(refreshed.revision, first.revision);
    assert_eq!(refreshed.updated_at, first.updated_at);
}

#[tokio::test]
async fn verified_creates_preserve_poll_cadence_and_retrieve_without_registering_again() {
    let fixture = Fixture::new().await;
    let mut provider = Provider::start();
    for (number, status, interval) in [
        ("VERIFIED-UNKNOWN", Status::Unknown, 1800),
        ("VERIFIED-OUT-FOR-DELIVERY", Status::OutForDelivery, 300),
    ] {
        let client = provider.client();
        let task = tokio::spawn(async move { client.register(number, None).await });
        let registration = provider.request().await;
        assert_eq!(registration.method, "POST");
        registration.tracker(number, status);
        let tracker = task.await.unwrap().unwrap();
        let shipment = fixture
            .store
            .create(
                CreateShipment {
                    title: "Verified shipment".into(),
                    merchant: None,
                    order_reference: None,
                    tracking: Tracking {
                        number: number.into(),
                        carrier: None,
                    },
                    source: None,
                },
                "test",
                number,
                Some(&tracker),
            )
            .await
            .unwrap();
        assert_eq!(shipment.tracking.carrier.as_deref(), Some("usps"));
        assert_eq!(shipment.sync.tracker_id.as_deref(), Some("trk_test"));
        assert_eq!(shipment.status, status);
        let remaining = delay(&fixture.store, shipment.id).await.unwrap();
        assert!(
            (interval - 1..=interval).contains(&remaining),
            "{status:?}: next poll in {remaining}s, expected {interval}s"
        );
        let worker = provider.worker(&fixture.store);
        assert!(!worker.tick().await.unwrap());
        due(&fixture.store, shipment.id).await;
        let request = observe(&mut provider, &worker, number, status).await;
        assert_eq!(request.method, "GET");
        assert_eq!(request.path, "/v2/trackers/trk_test");
        assert_eq!(
            fixture.store.get(shipment.id).await.unwrap().revision,
            shipment.revision
        );
        assert!(provider.incoming.try_recv().is_err());
    }
}

#[tokio::test]
async fn carrier_autofill_invalidates_stale_tracking_edits_and_preserves_verification() {
    let fixture = Fixture::new().await;
    let shipment = fixture.shipment("LEGACY-CARRIER").await;
    // Before carrier autofill, a valid registered tracker could have a known
    // status and tracker ID while its optional input carrier remained NULL.
    sqlx::query("UPDATE shipments SET tracking_carrier=NULL,carrier_status=?,tracker_id='trk_test' WHERE id=?")
        .bind(serde_json::to_string(&Status::Unknown).unwrap())
        .bind(shipment.id.to_string())
        .execute(fixture.store.pool())
        .await
        .unwrap();
    let stale = fixture.store.get(shipment.id).await.unwrap();
    let generation: i64 =
        sqlx::query_scalar("SELECT tracking_generation FROM shipments WHERE id=?")
            .bind(shipment.id.to_string())
            .fetch_one(fixture.store.pool())
            .await
            .unwrap();
    let mut provider = Provider::start();
    let worker = provider.worker(&fixture.store);
    let request = observe(&mut provider, &worker, "LEGACY-CARRIER", Status::Unknown).await;
    assert_eq!(request.method, "GET");
    let error = fixture
        .store
        .update(
            shipment.id,
            stale.revision,
            UpdateShipment {
                tracking: Patch {
                    value: Some(Some(stale.tracking)),
                },
                ..Default::default()
            },
            "owner",
            None,
        )
        .await
        .expect_err("the observed carrier must invalidate the earlier tracking edit");
    assert!(matches!(error, shipments::error::Error::Precondition));
    let current = fixture.store.get(shipment.id).await.unwrap();
    assert_eq!(current.revision, stale.revision + 1);
    assert_eq!(current.tracking.carrier.as_deref(), Some("usps"));
    assert_eq!(current.sync.tracker_id.as_deref(), Some("trk_test"));
    let current_generation: i64 =
        sqlx::query_scalar("SELECT tracking_generation FROM shipments WHERE id=?")
            .bind(shipment.id.to_string())
            .fetch_one(fixture.store.pool())
            .await
            .unwrap();
    assert_eq!(current_generation, generation);
    let events = fixture.store.events(shipment.id, 0, 100).await.unwrap();
    assert!(
        events
            .last()
            .unwrap()
            .detail
            .starts_with("Carrier tracking information updated")
    );
}

#[tokio::test]
async fn preserves_a_manual_terminal_override_set_during_network_io() {
    let fixture = Fixture::new().await;
    let shipment = fixture.shipment("TRACK2").await;
    let mut provider = Provider::start();
    let worker = provider.worker(&fixture.store);
    let task = start_tick(&worker);
    let request = provider.request().await;
    let corrected = fixture
        .store
        .set_override(
            shipment.id,
            shipment.revision,
            Some(SetOverride {
                status: Status::Cancelled,
                reason: "Order cancelled by the merchant".into(),
            }),
            "owner",
        )
        .await
        .unwrap();
    request.tracker("TRACK2", Status::InTransit);
    assert!(task.await.unwrap().unwrap());
    let observed = fixture.store.get(shipment.id).await.unwrap();
    assert_eq!(observed.status, Status::Cancelled);
    assert_eq!(observed.carrier_status, Some(Status::InTransit));
    assert_eq!(observed.status_override.unwrap().actor_id, "owner");
    assert_eq!(observed.revision, corrected.revision + 1);
    assert_eq!(observed.sync.next_attempt_at, None);
    assert!(!worker.tick().await.unwrap());
}

#[tokio::test]
async fn retry_schedule_and_attempts_survive_restart_without_exposing_provider_errors() {
    let fixture = Fixture::new().await;
    let shipment = fixture.shipment("TRACK3").await;
    let mut provider = Provider::start();
    let worker = provider.worker(&fixture.store);
    let task = start_tick(&worker);
    let request = provider.request().await;
    let mut reply = Reply::error(429);
    reply.headers.push(("Retry-After".into(), "600".into()));
    request.reply(reply);
    assert!(task.await.unwrap().unwrap());
    let failed = fixture.store.get(shipment.id).await.unwrap();
    assert_eq!(
        failed.sync.last_error.as_deref(),
        Some("EasyPost returned HTTP 429")
    );
    assert_eq!(failed.sync.attempts, 1);
    assert_eq!(failed.revision, shipment.revision);
    assert!((599..=600).contains(&delay(&fixture.store, shipment.id).await.unwrap()));
    fixture.store.close().await.unwrap();

    let reopened = Store::open(&fixture.path).await.unwrap();
    let worker = provider.worker(&reopened);
    assert!(!worker.tick().await.unwrap());
    assert_eq!(
        reopened
            .get(shipment.id)
            .await
            .unwrap()
            .sync
            .next_attempt_at,
        failed.sync.next_attempt_at
    );
    due(&reopened, shipment.id).await;
    let task = start_tick(&worker);
    provider.request().await.reply(Reply::error(503));
    assert!(task.await.unwrap().unwrap());
    assert_eq!(reopened.get(shipment.id).await.unwrap().sync.attempts, 2);
    assert!((119..=120).contains(&delay(&reopened, shipment.id).await.unwrap()));
    sqlx::query("UPDATE shipments SET attempts = 30, next_attempt_at = unixepoch() WHERE id = ?")
        .bind(shipment.id.to_string())
        .execute(reopened.pool())
        .await
        .unwrap();
    let task = start_tick(&worker);
    let mut reply = Reply::error(503);
    reply
        .headers
        .push(("Retry-After".into(), "9999999999".into()));
    provider.request().await.reply(reply);
    assert!(task.await.unwrap().unwrap());
    assert!((21599..=21600).contains(&delay(&reopened, shipment.id).await.unwrap()));
    due(&reopened, shipment.id).await;
    observe(&mut provider, &worker, "TRACK3", Status::InTransit).await;
    let recovered = reopened.get(shipment.id).await.unwrap();
    assert_eq!(recovered.sync.attempts, 0);
    assert!(recovered.sync.last_error.is_none());
}

#[tokio::test]
async fn a_response_from_a_replaced_tracking_identity_is_discarded() {
    let fixture = Fixture::new().await;
    let shipment = fixture.shipment("OLDTRACK").await;
    let mut provider = Provider::start();
    let worker = provider.worker(&fixture.store);
    let task = start_tick(&worker);
    let old_request = provider.request().await;
    let replaced = fixture
        .store
        .update(
            shipment.id,
            shipment.revision,
            UpdateShipment {
                tracking: Patch {
                    value: Some(Some(Tracking {
                        carrier: Some("USPS".into()),
                        number: "NEWTRACK".into(),
                    })),
                },
                ..Default::default()
            },
            "owner",
            None,
        )
        .await
        .unwrap();
    old_request.tracker("OLDTRACK", Status::Delivered);
    assert!(task.await.unwrap().unwrap());
    let current = fixture.store.get(shipment.id).await.unwrap();
    assert_eq!(current.revision, replaced.revision);
    assert_eq!(current.status, Status::Expected);
    assert!(current.carrier_status.is_none());
    assert!(current.sync.tracker_id.is_none());
    let request = observe(&mut provider, &worker, "NEWTRACK", Status::InTransit).await;
    assert_eq!(request.method, "POST");
}

#[tokio::test]
async fn archiving_an_in_flight_shipment_discards_the_response_and_stops_polling() {
    let fixture = Fixture::new().await;
    let shipment = fixture.shipment("TRACK4").await;
    let mut provider = Provider::start();
    let worker = provider.worker(&fixture.store);
    let task = start_tick(&worker);
    let request = provider.request().await;
    let archived = fixture
        .store
        .archive(shipment.id, shipment.revision, true, "owner")
        .await
        .unwrap();
    request.tracker("TRACK4", Status::Delivered);
    assert!(task.await.unwrap().unwrap());
    let current = fixture.store.get(shipment.id).await.unwrap();
    assert!(current.archived);
    assert_eq!(current.revision, archived.revision);
    assert!(current.carrier_status.is_none());
    assert!(current.sync.tracker_id.is_none());
    assert!(current.sync.next_attempt_at.is_none());
    assert!(!worker.tick().await.unwrap());
}

#[tokio::test]
async fn concurrent_workers_only_claim_a_due_shipment_once() {
    let fixture = Fixture::new().await;
    fixture.shipment("TRACK5").await;
    let mut provider = Provider::start();
    let worker = provider.worker(&fixture.store);
    let first = start_tick(&worker);
    let request = provider.request().await;
    assert!(!provider.worker(&fixture.store).tick().await.unwrap());
    request.tracker("TRACK5", Status::InTransit);
    assert!(first.await.unwrap().unwrap());
    assert!(provider.incoming.try_recv().is_err());
}

#[tokio::test]
async fn an_expired_claim_cannot_overwrite_a_newer_claim_for_the_same_generation() {
    let fixture = Fixture::new().await;
    let shipment = fixture.shipment("TRACK6").await;
    let mut provider = Provider::start();
    let worker = provider.worker(&fixture.store);
    let first = start_tick(&worker);
    let stale_request = provider.request().await;
    sqlx::query("UPDATE shipments SET lease_until = unixepoch() - 1 WHERE id = ?")
        .bind(shipment.id.to_string())
        .execute(fixture.store.pool())
        .await
        .unwrap();
    let second = start_tick(&worker);
    provider
        .request()
        .await
        .tracker("TRACK6", Status::OutForDelivery);
    assert!(second.await.unwrap().unwrap());
    let fresh = fixture.store.get(shipment.id).await.unwrap();
    stale_request.tracker("TRACK6", Status::Delivered);
    assert!(first.await.unwrap().unwrap());
    let current = fixture.store.get(shipment.id).await.unwrap();
    assert_eq!(current.status, Status::OutForDelivery);
    assert_eq!(current.revision, fresh.revision);
}

#[tokio::test]
async fn a_cancelled_claim_survives_restart_and_is_reclaimed_after_lease_expiry() {
    let fixture = Fixture::new().await;
    let shipment = fixture.shipment("TRACK7").await;
    let mut provider = Provider::start();
    let worker = provider.worker(&fixture.store);
    let task = start_tick(&worker);
    let request = provider.request().await;
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    drop(request);
    fixture.store.close().await.unwrap();
    let reopened = Store::open(&fixture.path).await.unwrap();
    let worker = provider.worker(&reopened);
    assert!(!worker.tick().await.unwrap());
    sqlx::query("UPDATE shipments SET lease_until = unixepoch() - 1 WHERE id = ?")
        .bind(shipment.id.to_string())
        .execute(reopened.pool())
        .await
        .unwrap();
    observe(&mut provider, &worker, "TRACK7", Status::Delivered).await;
    assert_eq!(
        reopened.get(shipment.id).await.unwrap().status,
        Status::Delivered
    );
}

#[tokio::test]
async fn terminal_refresh_permits_one_probe_and_a_failure_does_not_restart_polling() {
    let fixture = Fixture::new().await;
    let shipment = fixture.shipment("TRACK8").await;
    let mut provider = Provider::start();
    let worker = provider.worker(&fixture.store);
    observe(&mut provider, &worker, "TRACK8", Status::Delivered).await;
    let delivered = fixture.store.get(shipment.id).await.unwrap();
    assert!(!worker.tick().await.unwrap());
    fixture
        .store
        .refresh(shipment.id, delivered.revision, "owner")
        .await
        .unwrap();
    let task = start_tick(&worker);
    provider.request().await.reply(Reply::error(503));
    assert!(task.await.unwrap().unwrap());
    let failed = fixture.store.get(shipment.id).await.unwrap();
    assert_eq!(failed.status, Status::Delivered);
    assert_eq!(failed.revision, delivered.revision);
    assert_eq!(failed.sync.next_attempt_at, None);
    assert!(!worker.tick().await.unwrap());
    fixture
        .store
        .refresh(shipment.id, failed.revision, "owner")
        .await
        .unwrap();
    observe(&mut provider, &worker, "TRACK8", Status::Delivered).await;
    assert_eq!(
        fixture.store.get(shipment.id).await.unwrap().revision,
        delivered.revision
    );
}

#[tokio::test]
async fn clearing_a_correction_preserves_terminal_polling_and_latest_carrier_status() {
    let fixture = Fixture::new().await;
    let shipment = fixture.shipment("TRACK10").await;
    let mut provider = Provider::start();
    let worker = provider.worker(&fixture.store);
    observe(&mut provider, &worker, "TRACK10", Status::Delivered).await;
    let delivered = fixture.store.get(shipment.id).await.unwrap();
    let corrected = fixture
        .store
        .set_override(
            shipment.id,
            delivered.revision,
            Some(SetOverride {
                status: Status::Delivered,
                reason: "Physically received".into(),
            }),
            "human",
        )
        .await
        .unwrap();
    let cleared = fixture
        .store
        .set_override(shipment.id, corrected.revision, None, "human")
        .await
        .unwrap();
    assert_eq!(cleared.status, Status::Delivered);
    assert!(cleared.status_override.is_none());
    assert!(cleared.sync.next_attempt_at.is_none());
    assert!(!worker.tick().await.unwrap());
}

#[tokio::test]
async fn provider_client_bounds_responses_rejects_redirects_and_redacts_parse_errors() {
    assert!(EasyPostClient::for_loopback("key", "https://example.com/v2/").is_err());
    assert!(EasyPostClient::for_loopback("key", "http://user:password@127.0.0.1/v2/").is_err());
    assert!(EasyPostClient::new("").is_err());
    let mut provider = Provider::start();
    let client = provider.client();
    let task_client = client.clone();
    let task = tokio::spawn(async move { task_client.register("TRACK9", None).await });
    provider.request().await.reply(Reply {
        status: 302,
        headers: vec![(
            "Location".into(),
            format!("{}credential-capture", provider.base),
        )],
        body: "test-secret".into(),
    });
    let error = task.await.unwrap().unwrap_err();
    assert!(matches!(error, SyncError::Http { status: 302, .. }));
    assert_eq!(error.to_string(), "EasyPost returned HTTP 302");
    assert!(provider.incoming.try_recv().is_err());

    let task_client = client.clone();
    let task = tokio::spawn(async move { task_client.register("TRACK9", None).await });
    provider.request().await.reply(Reply {
        status: 200,
        headers: Vec::new(),
        body: "x".repeat(256 * 1024 + 1),
    });
    assert!(matches!(
        task.await.unwrap(),
        Err(SyncError::ResponseTooLarge)
    ));

    let task = tokio::spawn(async move { client.register("TRACK9", None).await });
    provider.request().await.reply(Reply {
        status: 200,
        headers: Vec::new(),
        body: "{test-secret: private-data}".into(),
    });
    assert_eq!(
        task.await.unwrap().unwrap_err().to_string(),
        "EasyPost returned an invalid tracker"
    );

    let client = provider.client();
    assert!(matches!(
        client.retrieve("../credential-capture", "TRACK9").await,
        Err(SyncError::InvalidResponse)
    ));
    let task = tokio::spawn(async move { client.retrieve("trk_requested", "TRACK9").await });
    provider
        .request()
        .await
        .tracker("TRACK9", Status::InTransit);
    assert!(matches!(
        task.await.unwrap(),
        Err(SyncError::InvalidResponse)
    ));
}

#[tokio::test]
async fn registration_accepts_unscanned_trackers_and_checks_the_returned_carrier() {
    let mut provider = Provider::start();
    let client = provider.client();
    for (status, requested_carrier) in [
        (Status::Unknown, None),
        (Status::PreTransit, Some("uSpS")),
        (Status::Error, Some("USPS")),
    ] {
        let client = client.clone();
        let task =
            tokio::spawn(async move { client.register("TRACK-NO-SCANS", requested_carrier).await });
        provider.request().await.tracker("TRACK-NO-SCANS", status);
        let tracker = task.await.unwrap().unwrap();
        assert_eq!(tracker.status, status);
        assert_eq!(tracker.carrier, "usps");
        assert!(tracker.public_url.is_none());
    }

    let task = tokio::spawn(async move { client.register("TRACK-NO-SCANS", Some("UPS")).await });
    provider
        .request()
        .await
        .tracker("TRACK-NO-SCANS", Status::Unknown);
    assert!(matches!(
        task.await.unwrap(),
        Err(SyncError::Rejected(TrackerRejection::CarrierCodeMismatch))
    ));
}

#[tokio::test]
async fn registration_retains_only_bounded_https_easypost_public_urls() {
    let mut provider = Provider::start();
    for (url, expected) in [
        (
            "https://track.easypost.com/public-tracker".into(),
            Some("https://track.easypost.com/public-tracker"),
        ),
        (
            "https://TRACK.EASYPOST.COM:443/public-tracker".into(),
            Some("https://track.easypost.com/public-tracker"),
        ),
        ("http://track.easypost.com/public-tracker".into(), None),
        (
            "https://track.easypost.com.example.invalid/tracker".into(),
            None,
        ),
        ("https://sub.track.easypost.com/tracker".into(), None),
        (
            "https://owner:private@track.easypost.com/tracker".into(),
            None,
        ),
        ("https://track.easypost.com:444/tracker".into(), None),
        ("javascript:private-data".into(), None),
        ("/relative-tracker".into(), None),
        (
            format!("https://track.easypost.com/{}", "x".repeat(2048)),
            None,
        ),
        (
            format!("https://track.easypost.com/{}", "🦆".repeat(200)),
            None,
        ),
    ] {
        let client = provider.client();
        let task = tokio::spawn(async move { client.register("TRACK-URL", None).await });
        provider.request().await.reply(Reply {
            status: 200,
            headers: Vec::new(),
            body: serde_json::to_string(&TrackerReply {
                id: "trk_test",
                tracking_code: "TRACK-URL",
                carrier: "USPS",
                status: Status::PreTransit,
                public_url: Some(&url),
            })
            .unwrap(),
        });
        let tracker = task.await.unwrap().unwrap();
        assert_eq!(tracker.public_url.as_deref(), expected);
        assert_eq!(tracker.status, Status::PreTransit);
    }
}

#[tokio::test]
async fn registration_requires_a_valid_carrier_and_tracker_contract() {
    let mut provider = Provider::start();
    for body in [
        r#"{"id":"trk_test","tracking_code":"TRACK","status":"unknown"}"#.into(),
        r#"{"id":"trk_test","tracking_code":"TRACK","status":"unknown","carrier":null}"#.into(),
        r#"{"id":"trk_test","tracking_code":"TRACK","status":"unknown","carrier":"   "}"#.into(),
        r#"{"id":"trk_test","tracking_code":"TRACK","status":"unknown","carrier":"USPS\nprivate"}"#
            .into(),
        serde_json::to_string(&TrackerReply {
            id: "trk_test",
            tracking_code: "TRACK",
            carrier: &"x".repeat(101),
            status: Status::Unknown,
            public_url: None,
        })
        .unwrap(),
        serde_json::to_string(&TrackerReply {
            id: "trk_test",
            tracking_code: "TRACK",
            carrier: "USPS",
            status: Status::Expected,
            public_url: None,
        })
        .unwrap(),
        serde_json::to_string(&TrackerReply {
            id: "trk_test",
            tracking_code: "DIFFERENT",
            carrier: "USPS",
            status: Status::Unknown,
            public_url: None,
        })
        .unwrap(),
    ] {
        let client = provider.client();
        let task = tokio::spawn(async move { client.register("TRACK", None).await });
        provider.request().await.reply(Reply {
            status: 200,
            headers: Vec::new(),
            body,
        });
        assert!(matches!(
            task.await.unwrap(),
            Err(SyncError::InvalidResponse)
        ));
    }
}

#[derive(Serialize)]
struct TrackerErrorReply<'a> {
    error: TrackerErrorDetail<'a>,
}

#[derive(Serialize)]
struct TrackerErrorDetail<'a> {
    code: &'a str,
    message: &'a str,
    errors: [&'a str; 1],
}

fn tracker_error_body(code: &str) -> String {
    serde_json::to_string(&TrackerErrorReply {
        error: TrackerErrorDetail {
            code,
            message: "test-secret and private tracking data",
            errors: ["more private request data"],
        },
    })
    .unwrap()
}

#[tokio::test]
async fn documented_tracker_rejections_are_typed_and_redact_provider_details() {
    let mut provider = Provider::start();
    for (index, (code, expected)) in [
        (
            "TRACKER.NO_CARRIER_FOR_CODE",
            TrackerRejection::NoCarrierForCode,
        ),
        (
            "TRACKER.CARRIER_CODE_MISMATCH",
            TrackerRejection::CarrierCodeMismatch,
        ),
        (
            "TRACKER.MULTIPLE_CARRIERS_FOR_CODE",
            TrackerRejection::MultipleCarriersForCode,
        ),
        ("TRACKER.INVALID_PARAMS", TrackerRejection::InvalidParams),
        (
            "TRACKER.INVALID_TEST_CODE",
            TrackerRejection::InvalidTestCode,
        ),
        (
            "TRACKER.NO_CARRIER_ACCOUNT",
            TrackerRejection::NoCarrierAccount,
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let client = provider.client();
        let task = tokio::spawn(async move { client.register("TRACK", None).await });
        provider.request().await.reply(Reply {
            status: if index % 2 == 0 { 400 } else { 422 },
            headers: Vec::new(),
            body: tracker_error_body(code),
        });
        let error = task.await.unwrap().unwrap_err();
        assert!(matches!(error, SyncError::Rejected(actual) if actual == expected));
        for text in [error.to_string(), format!("{error:?}")] {
            assert!(!text.contains("test-secret"));
            assert!(!text.contains("private"));
            assert!(!text.contains(code));
        }
    }
}

#[tokio::test]
async fn unknown_authentication_and_outage_errors_remain_generic_and_bounded() {
    let mut provider = Provider::start();
    for body in [
        tracker_error_body("TRACKER.RUN.ERROR"),
        tracker_error_body("private-provider-code"),
        "private malformed response".into(),
        r#"{"error":"private-data"}"#.into(),
    ] {
        let client = provider.client();
        let task = tokio::spawn(async move { client.register("TRACK", None).await });
        provider.request().await.reply(Reply {
            status: 422,
            headers: Vec::new(),
            body,
        });
        let error = task.await.unwrap().unwrap_err();
        assert!(matches!(
            error,
            SyncError::Http {
                status: 422,
                retry_after: None
            }
        ));
        assert_eq!(error.to_string(), "EasyPost returned HTTP 422");
        assert!(!format!("{error:?}").contains("private"));
    }
    for status in [401, 403, 429, 503] {
        let client = provider.client();
        let task = tokio::spawn(async move { client.register("TRACK", None).await });
        provider.request().await.reply(Reply {
            status,
            headers: vec![("Retry-After".into(), "600".into())],
            body: tracker_error_body("TRACKER.NO_CARRIER_FOR_CODE"),
        });
        let error = task.await.unwrap().unwrap_err();
        assert!(
            matches!(error, SyncError::Http { status: actual, retry_after: Some(600) } if actual == status)
        );
        assert_eq!(
            error.to_string(),
            format!("EasyPost returned HTTP {status}")
        );
    }
    for headers in [
        Vec::new(),
        vec![("Transfer-Encoding".into(), "chunked".into())],
    ] {
        let client = provider.client();
        let task = tokio::spawn(async move { client.register("TRACK", None).await });
        provider.request().await.reply(Reply {
            status: 422,
            headers,
            body: format!(
                "{}{}",
                tracker_error_body("TRACKER.NO_CARRIER_FOR_CODE"),
                " ".repeat(256 * 1024)
            ),
        });
        assert!(matches!(
            task.await.unwrap(),
            Err(SyncError::ResponseTooLarge)
        ));
    }
}

#[tokio::test]
async fn connection_check_reads_one_page_without_creating_trackers() {
    #[derive(Serialize)]
    struct Page {
        trackers: Vec<ListedTracker>,
        has_more: bool,
    }
    #[derive(Serialize)]
    struct ListedTracker {
        object: &'static str,
        id: &'static str,
        tracking_code: &'static str,
    }

    let mut provider = Provider::start();
    let client = provider.client();
    for trackers in [
        Vec::new(),
        vec![ListedTracker {
            object: "Tracker",
            id: "trk_private",
            tracking_code: "PRIVATE-TRACKING-NUMBER",
        }],
    ] {
        let client = client.clone();
        let task = tokio::spawn(async move { client.check_connection().await });
        let request = provider.request().await;
        assert_eq!(request.method, "GET");
        assert_eq!(request.path, "/v2/trackers?page_size=1");
        assert_eq!(
            request.authorization.as_deref(),
            Some("Basic dGVzdC1zZWNyZXQ6")
        );
        assert!(request.body.is_empty());
        request.reply(Reply {
            status: 200,
            headers: Vec::new(),
            body: serde_json::to_string(&Page {
                has_more: !trackers.is_empty(),
                trackers,
            })
            .unwrap(),
        });
        task.await.unwrap().unwrap();
        // The response's pagination flag cannot cause extra reads or writes.
        assert!(provider.incoming.try_recv().is_err());
    }
}

#[tokio::test]
async fn connection_check_redacts_authentication_errors_and_never_follows_redirects() {
    let mut provider = Provider::start();
    let capture = Provider::start();
    let client = provider.client();
    let task_client = client.clone();
    let task = tokio::spawn(async move { task_client.check_connection().await });
    provider.request().await.reply(Reply::error(401));
    let error = task.await.unwrap().unwrap_err();
    assert_eq!(error.to_string(), "EasyPost returned HTTP 401");
    assert!(!format!("{error:?}").contains("test-secret"));

    let task = tokio::spawn(async move { client.check_connection().await });
    provider.request().await.reply(Reply {
        status: 302,
        headers: vec![("Location".into(), capture.base.clone())],
        body: "test-secret and private account data".into(),
    });
    let error = task.await.unwrap().unwrap_err();
    assert_eq!(error.to_string(), "EasyPost returned HTTP 302");
    assert!(matches!(error, SyncError::Http { status: 302, .. }));
    assert!(capture.incoming.is_empty());
    assert!(provider.incoming.try_recv().is_err());
}

#[tokio::test]
async fn connection_check_rejects_invalid_and_oversized_success_responses() {
    let mut provider = Provider::start();
    let client = provider.client();
    for body in [
        "{test-secret: private-data}",
        "{}",
        r#"{"trackers":"private account data","has_more":false}"#,
        r#"{"trackers":[{"object":"User"}],"has_more":false}"#,
        r#"{"trackers":[{"object":"Tracker"},{"object":"Tracker"}],"has_more":false}"#,
        r#"{"trackers":[],"has_more":"test-secret"}"#,
    ] {
        let client = client.clone();
        let task = tokio::spawn(async move { client.check_connection().await });
        provider.request().await.reply(Reply {
            status: 200,
            headers: Vec::new(),
            body: body.into(),
        });
        let error = task.await.unwrap().unwrap_err();
        assert_eq!(
            error.to_string(),
            "EasyPost returned an invalid connection-check response"
        );
        assert!(matches!(error, SyncError::InvalidConnectionResponse));
    }
    for headers in [
        Vec::new(),
        vec![("Transfer-Encoding".into(), "chunked".into())],
    ] {
        let client = client.clone();
        let task = tokio::spawn(async move { client.check_connection().await });
        provider.request().await.reply(Reply {
            status: 200,
            headers,
            body: "test-secret".repeat(26 * 1024),
        });
        assert!(matches!(
            task.await.unwrap(),
            Err(SyncError::ResponseTooLarge)
        ));
    }
}
