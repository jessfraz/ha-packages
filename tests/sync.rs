use shipments::shipping_api::easypost::EasyPostClient;
use shipments::shipping_api::error::Rejection as TrackerRejection;
use shipments::shipping_api::error::{Error as ShippingError, ErrorKind};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use shipments::models::{
    CarrierEvent, CreateShipment, EventKind, InputTracking, Patch, SetOverride, Shipment, Status,
    TrackingLocation, UpdateShipment,
};
use shipments::store::Store;
use shipments::sync::SyncWorker;
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
                    direction: shipments::models::Direction::Inbound,
                    title: "Replacement cable".into(),
                    merchant: None,
                    order_reference: None,
                    signature_required: None,
                    tracking: InputTracking {
                        recipient_url: None,
                        carrier: Some("USPS".parse().unwrap()),
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

    fn empty_tracker_search(self, number: &str) {
        assert_eq!(self.method, "GET");
        let url = reqwest::Url::parse(&format!("http://provider{}", self.path)).unwrap();
        assert_eq!(url.path(), "/v2/trackers");
        let query: std::collections::HashMap<_, _> = url.query_pairs().collect();
        assert_eq!(
            query.get("tracking_codes[]").map(|value| value.as_ref()),
            Some(number)
        );
        assert_eq!(
            query.get("carrier").map(|value| value.as_ref()),
            Some("USPS")
        );
        assert!(query.contains_key("start_datetime"));
        assert!(query.contains_key("end_datetime"));
        assert!(self.body.is_empty());
        #[derive(Serialize)]
        struct EmptyPage {
            trackers: Vec<TrackerReply<'static>>,
            has_more: bool,
        }
        self.reply(Reply {
            status: 200,
            headers: Vec::new(),
            body: serde_json::to_string(&EmptyPage {
                trackers: Vec::new(),
                has_more: false,
            })
            .unwrap(),
        });
    }

    fn reconciled_tracker(self, number: &str, status: Status) {
        assert_eq!(self.method, "GET");
        assert!(self.path.contains("tracking_codes"));
        #[derive(Serialize)]
        struct TrackerPage<'a> {
            trackers: Vec<TrackerReply<'a>>,
            has_more: bool,
        }
        self.reply(Reply {
            status: 200,
            headers: Vec::new(),
            body: serde_json::to_string(&TrackerPage {
                trackers: vec![TrackerReply {
                    id: "trk_test",
                    tracking_code: number,
                    carrier: "USPS",
                    status,
                    public_url: None,
                }],
                has_more: false,
            })
            .unwrap(),
        });
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

#[derive(Serialize)]
struct DetailedTrackerReply<'a> {
    #[serde(flatten)]
    tracker: TrackerReply<'a>,
    tracking_details: Vec<CarrierEvent>,
    signed_by: Option<&'a str>,
    est_delivery_date: Option<&'a str>,
}

fn scan(at: &str, message: &str) -> CarrierEvent {
    CarrierEvent {
        status: Some(Status::InTransit),
        status_detail: Some("arrived_at_facility".into()),
        datetime: Some(at.into()),
        message: Some(message.into()),
        description: Some("Carrier scan details".into()),
        source: Some("USPS".into()),
        carrier_code: Some("01".into()),
        tracking_location: Some(TrackingLocation {
            city: Some("Example City".into()),
            state: Some("EX".into()),
            country: Some("US".into()),
            zip: Some("00000".into()),
        }),
    }
}

fn detailed_reply(
    number: &str,
    status: Status,
    scans: Vec<CarrierEvent>,
    signed_by: Option<&str>,
) -> Reply {
    Reply {
        status: 200,
        headers: Vec::new(),
        body: serde_json::to_string(&DetailedTrackerReply {
            tracker: TrackerReply {
                id: "trk_test",
                tracking_code: number,
                carrier: "USPS",
                status,
                public_url: None,
            },
            tracking_details: scans,
            signed_by,
            est_delivery_date: Some("2026-01-05T13:00:00-05:00"),
        })
        .unwrap(),
    }
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
        SyncWorker::new(store.pool().clone(), std::sync::Arc::new(self.client()))
    }

    async fn request(&mut self) -> Incoming {
        tokio::time::timeout(Duration::from_secs(3), self.incoming.recv())
            .await
            .unwrap()
            .unwrap()
    }

    async fn registration_after_empty_search(&mut self, number: &str) -> Incoming {
        self.request().await.empty_tracker_search(number);
        let registration = self.request().await;
        assert_eq!(registration.method, "POST");
        assert_eq!(registration.path, "/v2/trackers");
        registration
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
    let request = if request.path.starts_with("/v2/trackers?") {
        // New registrations must complete their filtered lookup before POST.
        // Already registered shipments go straight to their tracker GET.
        request.empty_tracker_search(number);
        let registration = provider.request().await;
        assert_eq!(registration.method, "POST");
        assert_eq!(registration.path, "/v2/trackers");
        registration
    } else {
        assert_eq!(request.method, "GET");
        assert_eq!(request.path, "/v2/trackers/trk_test");
        request
    };
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
    let request = provider.registration_after_empty_search("TRACK1").await;
    assert_eq!(request.method, "POST");
    assert_eq!(request.path, "/v2/trackers");
    assert_eq!(
        request.authorization.as_deref(),
        Some("Basic dGVzdC1zZWNyZXQ6")
    );
    let registration: Registration = serde_json::from_slice(&request.body).unwrap();
    assert_eq!(registration.tracker.tracking_code, "TRACK1");
    assert_eq!(registration.tracker.carrier, "USPS");
    request.tracker("TRACK1", Status::InTransit);
    assert!(task.await.unwrap().unwrap());
    let first = fixture.store.get(shipment.id).await.unwrap();
    assert_eq!(first.status, Status::InTransit);
    assert_eq!(first.revision, shipment.revision + 1);
    assert_eq!(first.sync.tracker_id.as_deref(), Some("trk_test"));
    assert!((1799..=1980).contains(&delay(&fixture.store, shipment.id).await.unwrap()));
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
    assert!((299..=330).contains(&delay(&fixture.store, shipment.id).await.unwrap()));
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
    provider
        .registration_after_empty_search("TRACK-URL")
        .await
        .reply(Reply {
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
    assert_eq!(first.tracking.carrier.as_str(), "USPS");
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
                    direction: shipments::models::Direction::Inbound,
                    title: "Verified shipment".into(),
                    merchant: None,
                    order_reference: None,
                    signature_required: None,
                    tracking: InputTracking {
                        recipient_url: None,
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
        assert_eq!(shipment.tracking.carrier.as_str(), "USPS");
        assert_eq!(shipment.sync.tracker_id.as_deref(), Some("trk_test"));
        assert_eq!(shipment.status, status);
        let remaining = delay(&fixture.store, shipment.id).await.unwrap();
        assert!(
            (interval - 1..=interval + interval / 10).contains(&remaining),
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
async fn canonical_carrier_changes_invalidate_stale_tracking_edits_and_preserve_verification() {
    let fixture = Fixture::new().await;
    let shipment = fixture.shipment("LEGACY-CARRIER").await;
    // A provider's canonical carrier observation must invalidate edits that
    // read the previous carrier while preserving the verified tracker.
    sqlx::query("UPDATE shipments SET tracking_carrier='ups',carrier_status=?,tracker_id='trk_test' WHERE id=?")
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
                    value: Some(Some(stale.tracking.into())),
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
    assert_eq!(current.tracking.carrier.as_str(), "USPS");
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
    let request = provider.registration_after_empty_search("TRACK2").await;
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
    let request = provider.registration_after_empty_search("TRACK3").await;
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
    let task = start_tick(&worker);
    provider
        .request()
        .await
        .reconciled_tracker("TRACK3", Status::InTransit);
    assert!(task.await.unwrap().unwrap());
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
    let old_request = provider.registration_after_empty_search("OLDTRACK").await;
    let replaced = fixture
        .store
        .update(
            shipment.id,
            shipment.revision,
            UpdateShipment {
                tracking: Patch {
                    value: Some(Some(InputTracking {
                        recipient_url: None,
                        carrier: Some("USPS".parse().unwrap()),
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
    let request = provider.registration_after_empty_search("TRACK4").await;
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
    let request = provider.registration_after_empty_search("TRACK5").await;
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
    let stale_request = provider.registration_after_empty_search("TRACK6").await;
    sqlx::query("UPDATE shipments SET lease_until = unixepoch() - 1 WHERE id = ?")
        .bind(shipment.id.to_string())
        .execute(fixture.store.pool())
        .await
        .unwrap();
    // A separate process may reclaim an expired lease. One client's shared
    // outbound slot prevents overlapping requests inside the same process.
    let second = start_tick(&provider.worker(&fixture.store));
    provider
        .request()
        .await
        .reconciled_tracker("TRACK6", Status::OutForDelivery);
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
    let request = provider.registration_after_empty_search("TRACK7").await;
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
    let task = start_tick(&worker);
    provider
        .request()
        .await
        .reconciled_tracker("TRACK7", Status::Delivered);
    assert!(task.await.unwrap().unwrap());
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
    assert!(!worker.tick().await.unwrap());
    due(&fixture.store, shipment.id).await;
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
    assert!(!worker.tick().await.unwrap());
    due(&fixture.store, shipment.id).await;
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
    assert!(matches!(error.kind, ErrorKind::Http { status: 302, .. }));
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
        Err(ShippingError {
            kind: ErrorKind::ResponseTooLarge,
            ..
        })
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
        Err(ShippingError {
            kind: ErrorKind::InvalidResponse,
            ..
        })
    ));
    let task = tokio::spawn(async move { client.retrieve("trk_requested", "TRACK9").await });
    provider
        .request()
        .await
        .tracker("TRACK9", Status::InTransit);
    assert!(matches!(
        task.await.unwrap(),
        Err(ShippingError {
            kind: ErrorKind::InvalidResponse,
            ..
        })
    ));
}

#[tokio::test]
async fn registration_accepts_unscanned_trackers_and_checks_the_returned_carrier() {
    let mut provider = Provider::start();
    let client = provider.client();
    for (status, requested_carrier) in [
        (Status::Unknown, None),
        (Status::PreTransit, Some("uSpS".parse().unwrap())),
        (Status::Error, Some("USPS".parse().unwrap())),
    ] {
        let client = client.clone();
        let task =
            tokio::spawn(async move { client.register("TRACK-NO-SCANS", requested_carrier).await });
        provider.request().await.tracker("TRACK-NO-SCANS", status);
        let tracker = task.await.unwrap().unwrap();
        assert_eq!(tracker.status, status);
        assert_eq!(tracker.carrier, shipments::carrier::Carrier::Usps);
        assert!(tracker.public_url.is_none());
    }

    let task = tokio::spawn(async move {
        client
            .register("TRACK-NO-SCANS", Some("UPS".parse().unwrap()))
            .await
    });
    provider
        .request()
        .await
        .tracker("TRACK-NO-SCANS", Status::Unknown);
    let error = task.await.unwrap().unwrap_err();
    assert!(matches!(
        error.kind,
        ErrorKind::Rejected(TrackerRejection::CarrierCodeMismatch)
    ));
    assert_eq!(error.provider_error_code(), None);
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
            Err(ShippingError {
                kind: ErrorKind::InvalidResponse,
                ..
            })
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
        assert!(
            matches!(error.kind, ErrorKind::ProviderRejected { rejection: actual, status, .. } if actual == expected && status == if index % 2 == 0 { 400 } else { 422 })
        );
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
            error.kind,
            ErrorKind::Http {
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
            body: tracker_error_body("private-provider-code"),
        });
        let error = task.await.unwrap().unwrap_err();
        assert!(
            matches!(error.kind, ErrorKind::Http { status: actual, retry_after: Some(600) } if actual == status)
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
        let error = task.await.unwrap().unwrap_err();
        assert!(matches!(error.kind, ErrorKind::Http { status: 422, .. }));
        assert_eq!(error.provider_error_code(), None);
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
    assert!(matches!(error.kind, ErrorKind::Http { status: 302, .. }));
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
        assert!(matches!(error.kind, ErrorKind::InvalidConnectionResponse));
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
            Err(ShippingError {
                kind: ErrorKind::ResponseTooLarge,
                ..
            })
        ));
    }
}

#[tokio::test]
async fn rate_limit_pauses_all_provider_operations_and_leaves_other_shipments_due() {
    let fixture = Fixture::new().await;
    let shipment = fixture.shipment("WAITING-TRACK").await;
    let mut provider = Provider::start();
    let client = provider.client();
    let worker = SyncWorker::new(
        fixture.store.pool().clone(),
        std::sync::Arc::new(client.clone()),
    );
    let task_client = client.clone();
    let task = tokio::spawn(async move { task_client.register("LIMITED-TRACK", None).await });
    let mut reply = Reply::error(429);
    reply.headers.push(("Retry-After".into(), "600".into()));
    provider.request().await.reply(reply);
    assert!(matches!(
        task.await.unwrap(),
        Err(ShippingError {
            kind: ErrorKind::Http {
                status: 429,
                retry_after: Some(600)
            },
            ..
        })
    ));

    let error = tokio::time::timeout(Duration::from_secs(1), client.register("NEW-TRACK", None))
        .await
        .expect("the shared cooldown must reject without another provider request")
        .unwrap_err();
    assert!(error.to_string().contains("paused"));
    assert!(client.retrieve("trk_test", "WAITING-TRACK").await.is_err());
    assert!(client.check_connection().await.is_err());
    assert!(!worker.tick().await.unwrap());
    let waiting = fixture.store.get(shipment.id).await.unwrap();
    assert_eq!(waiting.sync.attempts, 0);
    assert!(waiting.sync.last_error.is_none());
    assert!(provider.incoming.try_recv().is_err());

    // Move only the monotonic provider clock; the real HTTP server still runs.
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(601)).await;
    tokio::time::resume();
    observe(&mut provider, &worker, "WAITING-TRACK", Status::Unknown).await;

    let task_client = client.clone();
    let task = tokio::spawn(async move { task_client.register("LIMITED-AGAIN", None).await });
    provider.request().await.reply(Reply::error(429));
    assert!(matches!(
        task.await.unwrap(),
        Err(ShippingError {
            kind: ErrorKind::Http {
                status: 429,
                retry_after: Some(60)
            },
            ..
        })
    ));
    assert!(client.check_connection().await.is_err());
    assert!(provider.incoming.try_recv().is_err());
}

#[tokio::test]
async fn shared_provider_slot_bounds_concurrency_spaces_starts_and_releases_after_cancellation() {
    let mut provider = Provider::start();
    let client = provider.client();
    let task_client = client.clone();
    let first = tokio::spawn(async move { task_client.register("FIRST-TRACK", None).await });
    let request = provider.request().await;
    assert!(client.register("SECOND-TRACK", None).await.is_err());
    assert!(client.check_connection().await.is_err());
    assert!(provider.incoming.try_recv().is_err());
    first.abort();
    assert!(first.await.unwrap_err().is_cancelled());
    drop(request);

    let task_client = client.clone();
    let second = tokio::spawn(async move { task_client.register("SECOND-TRACK", None).await });
    let request = provider.request().await;
    let at = std::time::Instant::now();
    request.tracker("SECOND-TRACK", Status::Unknown);
    second.await.unwrap().unwrap();
    let third = tokio::spawn(async move { client.retrieve("trk_test", "SECOND-TRACK").await });
    provider
        .request()
        .await
        .tracker("SECOND-TRACK", Status::Unknown);
    third.await.unwrap().unwrap();
    assert!(at.elapsed() >= Duration::from_millis(900));
}

#[tokio::test]
async fn unscanned_labels_slow_down_until_carrier_status_changes() {
    let fixture = Fixture::new().await;
    let shipment = fixture.shipment("UNSCANNED-TRACK").await;
    let mut provider = Provider::start();
    let worker = provider.worker(&fixture.store);
    observe(&mut provider, &worker, "UNSCANNED-TRACK", Status::Unknown).await;
    for (age, status, base) in [
        (2 * 86400 - 10, Status::Unknown, 1800),
        (2 * 86400, Status::Unknown, 7200),
        (7 * 86400, Status::Unknown, 21600),
        (8 * 86400, Status::PreTransit, 1800),
        (8 * 86400, Status::PreTransit, 21600),
        (8 * 86400, Status::InTransit, 1800),
        (8 * 86400, Status::OutForDelivery, 300),
    ] {
        sqlx::query("UPDATE shipments SET status_since = datetime('now', '-' || ? || ' seconds'), next_attempt_at = unixepoch() WHERE id = ?")
            .bind(age).bind(shipment.id.to_string()).execute(fixture.store.pool()).await.unwrap();
        observe(&mut provider, &worker, "UNSCANNED-TRACK", status).await;
        let remaining = delay(&fixture.store, shipment.id).await.unwrap();
        assert!(
            (base - 1..=base + base / 10).contains(&remaining),
            "{status:?} after {age}s: {remaining}s"
        );
    }
}

#[tokio::test]
async fn provider_logs_correlate_requests_and_shipments_without_exposing_private_data() {
    #[derive(Clone, Default)]
    struct Capture(Arc<std::sync::Mutex<Vec<u8>>>);
    impl Write for Capture {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    #[derive(Deserialize)]
    struct Entry {
        provider: shipments::shipping_api::types::ProviderId,
        timestamp: String,
        event: String,
        request_id: uuid::Uuid,
        shipment_id: Option<uuid::Uuid>,
        operation: String,
        outcome: String,
        error_code: Option<String>,
        provider_error_code: Option<String>,
        provider_status: Option<u16>,
        provider_request_id: Option<uuid::Uuid>,
        duration_ms: u64,
    }
    let capture = Capture::default();
    let mut provider = Provider::start();
    let client = provider
        .client()
        .with_logger(shipments::logging::with_writer(capture.clone()));
    let request_id = uuid::Uuid::now_v7();
    let provider_request_id = uuid::Uuid::now_v7();
    let task_client = client.with_request_id(&request_id.to_string());
    let task =
        tokio::spawn(async move { task_client.register("PRIVATE-TRACKING-CODE", None).await });
    provider.request().await.reply(Reply {
        status: 404, headers: vec![("x-ep-request-uuid".into(), provider_request_id.to_string())],
        body: r#"{"error":{"code":"CREDENTIALS_NOT_FOUND","message":"PRIVATE-TRACKING-CODE test-secret PRIVATE-PROVIDER-DATA"}}"#.into(),
    });
    assert!(task.await.unwrap().is_err());

    let fixture = Fixture::new().await;
    let shipment = fixture.shipment("PRIVATE-TRACKING-CODE").await;
    let worker = SyncWorker::new(
        fixture.store.pool().clone(),
        std::sync::Arc::new(client.clone()),
    );
    observe(
        &mut provider,
        &worker,
        "PRIVATE-TRACKING-CODE",
        Status::Unknown,
    )
    .await;
    let owner = "logging-test-owner-credential-at-least-32-bytes";
    let server = shipments::api::start(
        "127.0.0.1:0".parse().unwrap(),
        Arc::new(shipments::api::Context::new(
            fixture.store.clone(),
            owner,
            None,
            Some(std::sync::Arc::new(client)),
        )),
    )
    .unwrap();
    let health = tokio::spawn(async move {
        reqwest::Client::new()
            .post(format!(
                "http://{}/v1/installation/check",
                server.local_addr()
            ))
            .bearer_auth(owner)
            .send()
            .await
            .map(|response| (server, response))
    });
    let mut reply = Reply::error(401);
    reply
        .headers
        .push(("x-ep-request-uuid".into(), "PRIVATE-PROVIDER-DATA".into()));
    provider.request().await.reply(reply);
    let (server, response) = health.await.unwrap().unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let health_request_id =
        uuid::Uuid::parse_str(response.headers()["x-request-id"].to_str().unwrap()).unwrap();
    server.close().await.unwrap();
    let bytes = capture.0.lock().unwrap().clone();
    let output = String::from_utf8(bytes.clone()).unwrap();
    for private in [
        "PRIVATE-TRACKING-CODE",
        "test-secret",
        "PRIVATE-PROVIDER-DATA",
        "trk_test",
        "127.0.0.1",
    ] {
        assert!(
            !output.contains(private),
            "private provider data reached logs"
        );
    }
    let entries: Vec<Entry> = serde_json::Deserializer::from_slice(&bytes)
        .into_iter::<Entry>()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(entries.len(), 4);
    assert_eq!(entries[0].event, "provider_request");
    assert_eq!(entries[0].request_id, request_id);
    assert_eq!(entries[0].operation, "register");
    assert_eq!(entries[0].outcome, "rejected");
    assert_eq!(
        entries[0].error_code.as_deref(),
        Some("tracking_carrier_credentials_required")
    );
    assert_eq!(entries[0].provider_status, Some(404));
    assert_eq!(entries[0].provider_request_id, Some(provider_request_id));
    assert_eq!(
        entries[0].provider_error_code.as_deref(),
        Some("CREDENTIALS_NOT_FOUND")
    );
    assert_eq!(entries[1].shipment_id, Some(shipment.id));
    assert_eq!(entries[1].operation, "reconcile");
    assert_eq!(entries[1].outcome, "success");
    assert!(entries[1].error_code.is_none());
    assert_eq!(entries[2].shipment_id, Some(shipment.id));
    assert_eq!(entries[2].request_id, entries[1].request_id);
    assert_eq!(entries[2].operation, "register");
    assert_eq!(entries[2].outcome, "success");
    assert!(entries[2].error_code.is_none());
    assert_eq!(entries[3].request_id, health_request_id);
    assert_eq!(entries[3].operation, "health");
    assert!(entries[3].provider_request_id.is_none());
    assert_eq!(entries[3].outcome, "error");
    for entry in entries {
        assert_eq!(
            entry.provider,
            shipments::shipping_api::types::ProviderId::EasyPost
        );
        time::OffsetDateTime::parse(
            &entry.timestamp,
            &time::format_description::well_known::Rfc3339,
        )
        .unwrap();
        assert!(entry.duration_ms < 20_000);
    }
}

#[tokio::test]
async fn carrier_history_merges_deduplicates_and_survives_restart_and_partial_snapshots() {
    let fixture = Fixture::new().await;
    let mut provider = Provider::start();
    let client = provider.client();
    let registration = tokio::spawn(async move { client.register("HISTORY-TRACK", None).await });
    let first = scan("2026-01-01T10:00:00Z", "Parcel accepted");
    let second = scan("2026-01-02T10:00:00Z", "Arrived at facility");
    provider.request().await.reply(detailed_reply(
        "HISTORY-TRACK",
        Status::InTransit,
        vec![second.clone(), first.clone(), first.clone()],
        None,
    ));
    let tracker = registration.await.unwrap().unwrap();
    let input = CreateShipment {
        direction: shipments::models::Direction::Inbound,
        title: "History test parcel".into(),
        merchant: None,
        order_reference: None,
        signature_required: Some(true),
        tracking: InputTracking {
            recipient_url: None,
            carrier: None,
            number: "HISTORY-TRACK".into(),
        },
        source: None,
    };
    let created = fixture
        .store
        .create(input.clone(), "owner", "history-create", Some(&tracker))
        .await
        .unwrap();
    assert_eq!(created.signature_required, Some(true));
    assert_eq!(
        created.carrier_tracking.events,
        vec![first.clone(), second.clone()]
    );
    assert_eq!(
        created.carrier_tracking.estimated_delivery_at.as_deref(),
        Some("2026-01-05T18:00:00Z")
    );
    assert!(created.carrier_tracking.delivered_at.is_none());
    let replay = fixture
        .store
        .create(input, "owner", "history-create", None)
        .await
        .unwrap();
    assert_eq!(replay.carrier_tracking, created.carrier_tracking);
    let worker = provider.worker(&fixture.store);
    let third = scan("2026-01-03T10:00:00Z", "Departed facility");
    let mut changed = created;
    for repeated in [false, true] {
        due(&fixture.store, changed.id).await;
        let task = start_tick(&worker);
        provider.request().await.reply(detailed_reply(
            "HISTORY-TRACK",
            Status::InTransit,
            vec![third.clone()],
            None,
        ));
        task.await.unwrap().unwrap();
        let actual = fixture.store.get(changed.id).await.unwrap();
        assert_eq!(
            actual.carrier_tracking.events,
            vec![first.clone(), second.clone(), third.clone()]
        );
        assert_eq!(actual.signature_required, Some(true));
        assert_eq!(actual.revision, changed.revision + i64::from(!repeated));
        changed = actual;
    }
    due(&fixture.store, changed.id).await;
    observe(&mut provider, &worker, "HISTORY-TRACK", Status::InTransit).await;
    assert_eq!(
        fixture
            .store
            .get(changed.id)
            .await
            .unwrap()
            .carrier_tracking,
        changed.carrier_tracking
    );
    assert_eq!(
        fixture.store.get(changed.id).await.unwrap().revision,
        changed.revision
    );
    let mut delivered_scan = scan("2026-01-04T10:00:00Z", "Signed and delivered");
    delivered_scan.status = Some(Status::Delivered);
    due(&fixture.store, changed.id).await;
    let task = start_tick(&worker);
    provider.request().await.reply(detailed_reply(
        "HISTORY-TRACK",
        Status::Delivered,
        vec![delivered_scan.clone()],
        Some("Parcel Recipient"),
    ));
    task.await.unwrap().unwrap();
    let delivered = fixture.store.get(changed.id).await.unwrap();
    assert_eq!(delivered.carrier_tracking.events.len(), 4);
    assert_eq!(
        delivered.carrier_tracking.signed_by.as_deref(),
        Some("Parcel Recipient")
    );
    assert_eq!(
        delivered.carrier_tracking.delivered_at.as_deref(),
        Some("2026-01-04T10:00:00Z")
    );
    assert_eq!(delivered.signature_required, Some(true));
    assert!(delivered.sync.next_attempt_at.is_none());
    let history = fixture.store.events(delivered.id, 0, 100).await.unwrap();
    assert_eq!(
        history.len(),
        3,
        "unchanged polls must not add history entries"
    );
    assert_eq!(
        history
            .last()
            .unwrap()
            .snapshot
            .as_ref()
            .unwrap()
            .carrier_tracking,
        delivered.carrier_tracking
    );
    drop(worker);
    fixture.store.close().await.unwrap();
    let reopened = Store::open(&fixture.path).await.unwrap();
    let restored = reopened.get(delivered.id).await.unwrap();
    assert_eq!(restored.carrier_tracking, delivered.carrier_tracking);
    assert_eq!(restored.signature_required, Some(true));
    reopened.close().await.unwrap();
}

#[tokio::test]
async fn tracking_replacement_clears_signature_and_history_and_fences_old_scans() {
    let fixture = Fixture::new().await;
    let shipment = fixture.shipment("OLD-HISTORY").await;
    let mut provider = Provider::start();
    let worker = provider.worker(&fixture.store);
    let task = start_tick(&worker);
    provider
        .registration_after_empty_search("OLD-HISTORY")
        .await
        .reply(detailed_reply(
            "OLD-HISTORY",
            Status::InTransit,
            vec![scan("2026-01-01T10:00:00Z", "Old parcel accepted")],
            None,
        ));
    task.await.unwrap().unwrap();
    let current = fixture.store.get(shipment.id).await.unwrap();
    let current = fixture
        .store
        .update(
            current.id,
            current.revision,
            UpdateShipment {
                signature_required: Patch {
                    value: Some(Some(true)),
                },
                ..Default::default()
            },
            "owner",
            None,
        )
        .await
        .unwrap();
    due(&fixture.store, current.id).await;
    let old_tick = start_tick(&worker);
    let pending = provider.request().await;
    let replaced = fixture
        .store
        .update(
            current.id,
            current.revision,
            UpdateShipment {
                tracking: Patch {
                    value: Some(Some(InputTracking {
                        recipient_url: None,
                        carrier: Some("usps".parse().unwrap()),
                        number: "NEW-HISTORY".into(),
                    })),
                },
                ..Default::default()
            },
            "owner",
            None,
        )
        .await
        .unwrap();
    assert!(replaced.signature_required.is_none());
    assert!(replaced.carrier_tracking.events.is_empty());
    assert!(replaced.carrier_tracking.estimated_delivery_at.is_none());
    pending.reply(detailed_reply(
        "OLD-HISTORY",
        Status::Delivered,
        vec![scan("2026-01-02T10:00:00Z", "Old parcel delivered")],
        Some("Old Recipient"),
    ));
    old_tick.await.unwrap().unwrap();
    assert!(
        fixture
            .store
            .get(current.id)
            .await
            .unwrap()
            .carrier_tracking
            .events
            .is_empty()
    );
    let explicit = fixture
        .store
        .update(
            replaced.id,
            replaced.revision,
            UpdateShipment {
                tracking: Patch {
                    value: Some(Some(InputTracking {
                        recipient_url: None,
                        carrier: Some("usps".parse().unwrap()),
                        number: "ANOTHER-HISTORY".into(),
                    })),
                },
                signature_required: Patch {
                    value: Some(Some(false)),
                },
                ..Default::default()
            },
            "owner",
            None,
        )
        .await
        .unwrap();
    assert_eq!(explicit.signature_required, Some(false));
    let cleared = fixture
        .store
        .update(
            explicit.id,
            explicit.revision,
            UpdateShipment {
                signature_required: Patch { value: Some(None) },
                ..Default::default()
            },
            "owner",
            None,
        )
        .await
        .unwrap();
    assert!(cleared.signature_required.is_none());
}

#[tokio::test]
async fn optional_carrier_metadata_is_bounded_and_bad_fields_do_not_reject_tracking() {
    let mut provider = Provider::start();
    let client = provider.client();
    let malformed = tokio::spawn(async move { client.register("MALFORMED-HISTORY", None).await });
    provider.request().await.reply(Reply {
        status: 200, headers: Vec::new(),
        body: r#"{"id":"trk_test","tracking_code":"MALFORMED-HISTORY","carrier":"USPS","status":"unknown","signed_by":42,"est_delivery_date":"not a date","tracking_details":[null,42,{"message":"Valid scan despite malformed optional fields","status":"future_status","datetime":false,"tracking_location":{"city":"Example City","zip":42}}]}"#.into(),
    });
    let tracker = malformed.await.unwrap().unwrap();
    assert_eq!(tracker.status, Status::Unknown);
    assert!(tracker.carrier_tracking.signed_by.is_none());
    assert!(tracker.carrier_tracking.estimated_delivery_at.is_none());
    assert_eq!(tracker.carrier_tracking.events.len(), 1);
    let event = &tracker.carrier_tracking.events[0];
    assert!(event.status.is_none());
    assert!(event.datetime.is_none());
    assert_eq!(
        event.tracking_location.as_ref().unwrap().city.as_deref(),
        Some("Example City")
    );
    assert!(event.tracking_location.as_ref().unwrap().zip.is_none());
    let client = provider.client();
    let bounded = tokio::spawn(async move { client.register("BOUNDED-HISTORY", None).await });
    let mut scans: Vec<_> = (0..600)
        .map(|index| {
            scan(
                &format!("2026-01-01T00:{:02}:{:02}Z", index / 60, index % 60),
                "Parcel scan",
            )
        })
        .collect();
    scans[599].message = Some("🦀".repeat(1000));
    scans[599].description = Some("x".repeat(10000));
    provider.request().await.reply(detailed_reply(
        "BOUNDED-HISTORY",
        Status::PreTransit,
        scans,
        Some(&"x".repeat(1000)),
    ));
    let tracker = bounded.await.unwrap().unwrap();
    assert_eq!(tracker.carrier_tracking.events.len(), 512);
    assert!(tracker.carrier_tracking.events_truncated);
    assert_eq!(
        tracker.carrier_tracking.events[0].datetime.as_deref(),
        Some("2026-01-01T00:01:28Z")
    );
    let last = tracker.carrier_tracking.events.last().unwrap();
    assert!(last.message.as_ref().unwrap().len() <= 1024);
    assert!(last.description.as_ref().unwrap().len() <= 2048);
    assert_eq!(
        tracker.carrier_tracking.signed_by.as_ref().unwrap().len(),
        200
    );
}

#[tokio::test]
async fn carrier_credentials_404_explains_carrier_setup() {
    const NUMBER: &str = "TRACK404";
    let mut provider = Provider::start();
    let client = provider.client();
    let task = tokio::spawn(async move { client.register(NUMBER, None).await });
    provider.request().await.reply(Reply {
        status: 404,
        headers: Vec::new(),
        body: tracker_error_body("CREDENTIALS_NOT_FOUND"),
    });
    let error = task.await.unwrap().unwrap_err();
    assert!(
        error
            .to_string()
            .contains("Configure this carrier in EasyPost"),
        "carrier credential failures must explain how to fix setup: {error}"
    );
}

#[tokio::test]
async fn native_error_codes_preserve_http_status_and_retry_advice_on_all_error_responses() {
    let mut provider = Provider::start();
    for status in [400, 404, 422, 429, 503] {
        let client = provider.client();
        let active = client.clone();
        let task = tokio::spawn(async move { active.register("TRACK404", None).await });
        provider.request().await.reply(Reply {
            status,
            headers: vec![("Retry-After".into(), "600".into())],
            body: tracker_error_body("CREDENTIALS_NOT_FOUND"),
        });
        let error = task.await.unwrap().unwrap_err();
        assert_eq!(error.provider_status(), Some(status));
        assert_eq!(error.provider_error_code(), Some("CREDENTIALS_NOT_FOUND"));
        assert_eq!(
            error.error_code(),
            match status {
                429 => "tracking_verification_rate_limited",
                503 => "provider_http",
                _ => "tracking_carrier_credentials_required",
            }
        );
        assert!(matches!(
            error.kind,
            ErrorKind::ProviderRejected {
                retry_after: Some(600),
                ..
            }
        ));
        assert!(matches!(
            client.check_connection().await,
            Err(ShippingError {
                kind: ErrorKind::Throttled {
                    retry_after: 599..=601
                },
                ..
            })
        ));
        assert!(provider.incoming.try_recv().is_err());
    }
}

#[tokio::test]
async fn transient_http_statuses_take_precedence_over_hard_tracking_rejection_codes() {
    let mut provider = Provider::start();
    for (native, rejection) in [
        (
            "TRACKER.NO_CARRIER_FOR_CODE",
            TrackerRejection::NoCarrierForCode,
        ),
        ("TRACKER.INVALID_PARAMS", TrackerRejection::InvalidParams),
    ] {
        for status in [401, 403, 408, 425, 429, 500, 502, 503, 504] {
            let client = provider.client();
            let task = tokio::spawn(async move { client.register("TRANSIENT123", None).await });
            provider.request().await.reply(Reply {
                status,
                headers: Vec::new(),
                body: tracker_error_body(native),
            });
            let error = task.await.unwrap().unwrap_err();
            assert!(
                !error.rejects_registration(),
                "HTTP {status} must not permanently reject tracking"
            );
            assert_eq!(error.provider_status(), Some(status));
            assert_eq!(error.provider_error_code(), Some(native));
            assert_eq!(
                error.to_string(),
                format!("EasyPost returned HTTP {status}")
            );
            assert_eq!(
                error.error_code(),
                match status {
                    401 | 403 => "tracking_provider_credentials",
                    429 => "tracking_verification_rate_limited",
                    _ => "provider_http",
                }
            );
        }
        for status in [400, 404, 422] {
            let client = provider.client();
            let task = tokio::spawn(async move { client.register("REJECTED123", None).await });
            provider.request().await.reply(Reply {
                status,
                headers: Vec::new(),
                body: tracker_error_body(native),
            });
            let error = task.await.unwrap().unwrap_err();
            assert!(error.rejects_registration());
            assert_eq!(error.error_code(), rejection.error_code());
            assert_eq!(error.provider_status(), Some(status));
            assert_eq!(error.provider_error_code(), Some(native));
        }
    }
}
