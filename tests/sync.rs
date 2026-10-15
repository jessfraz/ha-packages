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
use shipments::sync::{EasyPostClient, SyncError, SyncWorker};
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
                    tracking: Some(Tracking {
                        carrier: Some("USPS".into()),
                        number: number.into(),
                    }),
                    source: None,
                },
                "test",
                number,
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
    status: Status,
}

impl Reply {
    fn tracker(number: &str, status: Status) -> Self {
        Self {
            status: 200,
            headers: Vec::new(),
            body: serde_json::to_string(&TrackerReply {
                id: "trk_test",
                tracking_code: number,
                status,
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
                                        let headers = reply.headers.iter().map(|(key, value)| format!("{key}: {value}\r\n")).collect::<String>();
                                        let head = format!("HTTP/1.1 {} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n{}\r\n", reply.status, reply.body.len(), headers);
                                        if stream.write_all(head.as_bytes()).is_ok() { let _ = stream.write_all(reply.body.as_bytes()); }
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
                        carrier: Some("UPS".into()),
                        number: "NEWTRACK".into(),
                    })),
                },
                ..Default::default()
            },
            "owner",
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
