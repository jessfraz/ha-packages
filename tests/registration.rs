use shipments::shipping_api::easypost::EasyPostClient;
use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::Duration,
};

use serde::{Deserialize, Serialize};
use shipments::{
    models::{
        CreateShipment, Direction, InputTracking, Patch, RegistrationState, Shipment, Status,
        UpdateShipment,
    },
    store::Store,
    sync::SyncWorker,
};
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

const NUMBER: &str = "9400100000000000000001";
const REPLACEMENT: &str = "9400100000000000000002";

struct Fixture {
    _directory: tempfile::TempDir,
    path: PathBuf,
    store: Store,
}
impl Fixture {
    async fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("registration.sqlite3");
        let store = Store::open(&path).await.unwrap();
        Self {
            _directory: directory,
            path,
            store,
        }
    }
    async fn shipment(&self) -> Shipment {
        self.store
            .create(
                CreateShipment {
                    direction: Direction::Inbound,
                    title: "Registration test parcel".into(),
                    merchant: None,
                    order_reference: None,
                    signature_required: None,
                    tracking: InputTracking {
                        recipient_url: None,
                        number: NUMBER.into(),
                        carrier: Some("USPS".parse().unwrap()),
                    },
                    source: None,
                },
                "test",
                "registration",
                None,
            )
            .await
            .unwrap()
    }
    async fn due(&self, id: uuid::Uuid) {
        sqlx::query("UPDATE shipments SET next_attempt_at=0,lease_until=0 WHERE id=?")
            .bind(id.to_string())
            .execute(self.store.pool())
            .await
            .unwrap();
    }
}

struct Incoming {
    method: String,
    target: String,
    body: Vec<u8>,
    reply: mpsc::Sender<(u16, String)>,
}
impl Incoming {
    fn respond(self, status: u16, body: String) {
        let _ = self.reply.send((status, body));
    }
    fn tracker(self) {
        self.respond(200, serde_json::to_string(&tracker(NUMBER)).unwrap());
    }
    fn page(self, trackers: Vec<TrackerReply>, has_more: bool) {
        self.respond(
            200,
            serde_json::to_string(&TrackerPage { trackers, has_more }).unwrap(),
        );
    }
    fn query(&self) -> Vec<(String, String)> {
        reqwest::Url::parse(&format!("http://localhost{}", self.target))
            .unwrap()
            .query_pairs()
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect()
    }
}

#[derive(Serialize)]
struct TrackerReply {
    id: String,
    tracking_code: String,
    carrier: String,
    status: Status,
    created_at: String,
}
fn tracker(number: &str) -> TrackerReply {
    TrackerReply {
        id: "trk_registration".into(),
        tracking_code: number.into(),
        carrier: "USPS".into(),
        status: Status::Unknown,
        created_at: "2026-01-01T00:00:00Z".into(),
    }
}
#[derive(Serialize)]
struct TrackerPage {
    trackers: Vec<TrackerReply>,
    has_more: bool,
}
#[derive(Deserialize)]
struct RegistrationRequest {
    tracker: RegistrationTracking,
}
#[derive(Deserialize, Debug, PartialEq, Eq)]
struct RegistrationTracking {
    tracking_code: String,
    carrier: String,
}

// Actual loopback HTTP with response gates: cancellation, restarts, durable state,
// and registration/reconciliation are exercised through the production worker.
struct Provider {
    base: String,
    incoming: UnboundedReceiver<Incoming>,
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}
impl Provider {
    fn new() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}/v2/", listener.local_addr().unwrap());
        listener.set_nonblocking(true).unwrap();
        let (sender, incoming) = unbounded_channel();
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        let thread = thread::spawn(move || {
            while !stopping.load(Ordering::Relaxed) {
                let (stream, _) = match listener.accept() {
                    Ok(connection) => connection,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2));
                        continue;
                    }
                    Err(_) => return,
                };
                let Some((mut stream, method, target, body)) = read_request(stream) else {
                    continue;
                };
                let (reply, receiver) = mpsc::channel();
                if sender
                    .send(Incoming {
                        method,
                        target,
                        body,
                        reply,
                    })
                    .is_err()
                {
                    return;
                }
                while !stopping.load(Ordering::Relaxed) {
                    match receiver.recv_timeout(Duration::from_millis(20)) {
                        Ok((status, body)) => {
                            let _ = write!(
                                stream,
                                "HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                                body.len()
                            );
                            break;
                        }
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    }
                }
            }
        });
        Self {
            base,
            incoming,
            stop,
            thread: Some(thread),
        }
    }
    fn worker(&self, store: &Store) -> SyncWorker {
        SyncWorker::new(
            store.pool().clone(),
            std::sync::Arc::new(
                EasyPostClient::for_loopback("registration-test-key", &self.base).unwrap(),
            ),
        )
    }
    async fn request(&mut self) -> Incoming {
        tokio::time::timeout(Duration::from_secs(4), self.incoming.recv())
            .await
            .unwrap()
            .unwrap()
    }
    async fn empty_lookup(&mut self) {
        let request = self.request().await;
        assert_eq!(request.method, "GET");
        request.page(Vec::new(), false);
    }
    fn no_request(&mut self) {
        assert!(self.incoming.try_recv().is_err());
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
fn read_request(mut stream: TcpStream) -> Option<(TcpStream, String, String, Vec<u8>)> {
    stream.set_nonblocking(false).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
    let mut header = Vec::new();
    while !header.ends_with(b"\r\n\r\n") {
        let mut byte = [0];
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
    let target = request.next()?.to_owned();
    let length = lines
        .filter_map(|line| line.split_once(':'))
        .find(|(key, _)| key.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.trim().parse::<usize>().ok())
        .unwrap_or(0);
    if length > 16 * 1024 {
        return None;
    }
    let mut body = vec![0; length];
    stream.read_exact(&mut body).ok()?;
    Some((stream, method, target, body))
}
fn spawn(worker: SyncWorker) -> tokio::task::JoinHandle<Result<bool, shipments::error::Error>> {
    tokio::spawn(async move { worker.tick().await })
}

#[tokio::test]
async fn first_attempt_adopts_existing_tracker_without_a_saved_registration_intent() {
    let fixture = Fixture::new().await;
    let shipment = fixture.shipment().await;
    assert!(shipment.sync.registration_started_at.is_none());
    let mut provider = Provider::new();
    let task = spawn(provider.worker(&fixture.store));
    let lookup = provider.request().await;
    assert_eq!(lookup.method, "GET");
    let query = lookup.query();
    assert!(query.contains(&("tracking_codes[]".into(), NUMBER.into())));
    assert!(query.contains(&("carrier".into(), "USPS".into())));
    lookup.page(vec![tracker(NUMBER)], false);
    assert!(task.await.unwrap().unwrap());
    provider.no_request();
    let adopted = fixture.store.get(shipment.id).await.unwrap();
    assert_eq!(
        adopted.sync.registration_state,
        RegistrationState::Registered
    );
    assert_eq!(adopted.sync.tracker_id.as_deref(), Some("trk_registration"));
    assert!(adopted.sync.registration_started_at.is_some());
    assert!(adopted.sync.last_error.is_none());
    fixture.due(shipment.id).await;
    let task = spawn(provider.worker(&fixture.store));
    let retrieve = provider.request().await;
    assert_eq!(retrieve.method, "GET");
    assert_eq!(retrieve.target, "/v2/trackers/trk_registration");
    retrieve.tracker();
    assert!(task.await.unwrap().unwrap());
    provider.no_request();
    fixture.store.close().await.unwrap();
}

#[tokio::test]
async fn complete_empty_first_lookup_registers_once_with_the_requested_identity() {
    let fixture = Fixture::new().await;
    let shipment = fixture.shipment().await;
    let mut provider = Provider::new();
    let task = spawn(provider.worker(&fixture.store));
    provider.empty_lookup().await;
    let registration = provider.request().await;
    assert_eq!(registration.method, "POST");
    let submitted: RegistrationRequest = serde_json::from_slice(&registration.body).unwrap();
    assert_eq!(
        submitted.tracker,
        RegistrationTracking {
            tracking_code: NUMBER.into(),
            carrier: "USPS".into(),
        }
    );
    registration.respond(201, serde_json::to_string(&tracker(NUMBER)).unwrap());
    assert!(task.await.unwrap().unwrap());
    provider.no_request();
    let saved = fixture.store.get(shipment.id).await.unwrap();
    assert_eq!(saved.sync.registration_state, RegistrationState::Registered);
    assert_eq!(saved.sync.tracker_id.as_deref(), Some("trk_registration"));
    assert!(saved.sync.last_error.is_none());
    fixture.store.close().await.unwrap();
}

#[tokio::test]
async fn unavailable_first_lookup_keeps_intent_and_restart_adopts_without_a_post() {
    let fixture = Fixture::new().await;
    let shipment = fixture.shipment().await;
    let mut provider = Provider::new();
    let worker = provider.worker(&fixture.store);
    let task = spawn(worker.clone());
    let lookup = provider.request().await;
    assert_eq!(lookup.method, "GET");
    lookup.respond(503, "private upstream details".into());
    assert!(task.await.unwrap().unwrap());
    provider.no_request();
    let pending = fixture.store.get(shipment.id).await.unwrap();
    let started = pending.sync.registration_started_at.unwrap();
    assert_eq!(pending.sync.registration_state, RegistrationState::Pending);
    assert_eq!(pending.sync.error_code.as_deref(), Some("provider_http"));
    assert_eq!(pending.sync.provider_status, Some(503));
    assert_eq!(pending.sync.attempts, 1);
    assert_eq!(
        pending.sync.last_error.as_deref(),
        Some("EasyPost returned HTTP 503")
    );
    assert!(!worker.tick().await.unwrap());
    fixture.store.close().await.unwrap();
    let restored = Store::open(&fixture.path).await.unwrap();
    assert_eq!(
        restored
            .get(shipment.id)
            .await
            .unwrap()
            .sync
            .registration_started_at,
        Some(started)
    );
    sqlx::query("UPDATE shipments SET next_attempt_at=0,lease_until=0 WHERE id=?")
        .bind(shipment.id.to_string())
        .execute(restored.pool())
        .await
        .unwrap();
    let task = spawn(provider.worker(&restored));
    let lookup = provider.request().await;
    assert_eq!(lookup.method, "GET");
    lookup.page(vec![tracker(NUMBER)], false);
    assert!(task.await.unwrap().unwrap());
    provider.no_request();
    let adopted = restored.get(shipment.id).await.unwrap();
    assert_eq!(
        adopted.sync.registration_state,
        RegistrationState::Registered
    );
    assert_eq!(adopted.sync.tracker_id.as_deref(), Some("trk_registration"));
    assert_eq!(adopted.sync.registration_started_at, Some(started));
    assert!(adopted.sync.last_error.is_none());
    restored.close().await.unwrap();
}

#[tokio::test]
async fn unavailable_registration_with_a_rejection_code_retries_and_adopts_after_restart() {
    let fixture = Fixture::new().await;
    let shipment = fixture.shipment().await;
    let mut provider = Provider::new();
    let task = spawn(provider.worker(&fixture.store));
    provider.empty_lookup().await;
    let registration = provider.request().await;
    assert_eq!(registration.method, "POST");
    registration.respond(503, r#"{"error":{"code":"TRACKER.NO_CARRIER_FOR_CODE","message":"private upstream diagnostic"}}"#.into());
    assert!(task.await.unwrap().unwrap());
    let pending = fixture.store.get(shipment.id).await.unwrap();
    assert_eq!(pending.sync.registration_state, RegistrationState::Pending);
    assert_eq!(pending.sync.error_code.as_deref(), Some("provider_http"));
    assert_eq!(pending.sync.provider_status, Some(503));
    assert_eq!(
        pending.sync.provider_error_code.as_deref(),
        Some("TRACKER.NO_CARRIER_FOR_CODE")
    );
    assert_eq!(
        pending.sync.last_error.as_deref(),
        Some("EasyPost returned HTTP 503")
    );
    assert!(pending.sync.next_attempt_at.is_some());
    assert!(pending.sync.tracker_id.is_none());
    fixture.store.close().await.unwrap();
    let reopened = Store::open(&fixture.path).await.unwrap();
    sqlx::query("UPDATE shipments SET next_attempt_at=0,lease_until=0 WHERE id=?")
        .bind(shipment.id.to_string())
        .execute(reopened.pool())
        .await
        .unwrap();
    let task = spawn(provider.worker(&reopened));
    let lookup = provider.request().await;
    assert_eq!(lookup.method, "GET");
    lookup.page(vec![tracker(NUMBER)], false);
    assert!(task.await.unwrap().unwrap());
    provider.no_request();
    let adopted = reopened.get(shipment.id).await.unwrap();
    assert_eq!(adopted.sync.tracker_id.as_deref(), Some("trk_registration"));
    assert_eq!(
        adopted.sync.registration_state,
        RegistrationState::Registered
    );
    assert_eq!(
        adopted.sync.registration_started_at,
        pending.sync.registration_started_at
    );
    assert!(adopted.sync.last_error.is_none());
    reopened.close().await.unwrap();
}

#[tokio::test]
async fn carrier_configuration_failure_stops_until_an_explicit_refresh() {
    let fixture = Fixture::new().await;
    let shipment = fixture.shipment().await;
    let mut provider = Provider::new();
    let worker = provider.worker(&fixture.store);
    let task = spawn(worker.clone());
    provider.empty_lookup().await;
    provider.request().await.respond(
        404,
        r#"{"error":{"code":"CREDENTIALS_NOT_FOUND","message":"private diagnostic"}}"#.into(),
    );
    assert!(task.await.unwrap().unwrap());
    let blocked = fixture.store.get(shipment.id).await.unwrap();
    assert_eq!(
        blocked.sync.registration_state,
        RegistrationState::ConfigurationRequired
    );
    assert_eq!(
        blocked.sync.error_code.as_deref(),
        Some("tracking_carrier_credentials_required")
    );
    assert!(blocked.sync.next_attempt_at.is_none());
    assert!(!worker.tick().await.unwrap());
    provider.no_request();
    let retry = fixture
        .store
        .refresh(blocked.id, blocked.revision, "owner")
        .await
        .unwrap();
    assert_eq!(retry.sync.registration_state, RegistrationState::Pending);
    assert!(retry.sync.next_attempt_at.is_some());
    sqlx::query("UPDATE shipments SET next_attempt_at=0 WHERE id=?")
        .bind(retry.id.to_string())
        .execute(fixture.store.pool())
        .await
        .unwrap();
    let task = spawn(worker);
    let lookup = provider.request().await;
    assert_eq!(lookup.method, "GET");
    lookup.page(vec![tracker(NUMBER)], false);
    assert!(task.await.unwrap().unwrap());
    provider.no_request();
    let restored = fixture.store.get(retry.id).await.unwrap();
    assert_eq!(
        restored.sync.registration_state,
        RegistrationState::Registered
    );
    assert!(restored.sync.last_error.is_none());
    fixture.store.close().await.unwrap();
}

#[tokio::test]
async fn ambiguous_or_incomplete_first_lookup_never_registers() {
    for has_more in [false, true] {
        let fixture = Fixture::new().await;
        let shipment = fixture.shipment().await;
        let mut provider = Provider::new();
        let task = spawn(provider.worker(&fixture.store));
        let lookup = provider.request().await;
        assert_eq!(lookup.method, "GET");
        let candidates = if has_more {
            vec![tracker(NUMBER)]
        } else {
            vec![tracker(NUMBER), tracker(NUMBER)]
        };
        lookup.page(candidates, has_more);
        assert!(task.await.unwrap().unwrap());
        provider.no_request();
        let unresolved = fixture.store.get(shipment.id).await.unwrap();
        assert!(unresolved.sync.tracker_id.is_none());
        assert!(unresolved.sync.registration_started_at.is_some());
        assert_eq!(
            unresolved.sync.registration_state,
            RegistrationState::Rejected
        );
        assert!(unresolved.sync.next_attempt_at.is_none());
        assert_eq!(
            unresolved.sync.error_code.as_deref(),
            Some("registration_ambiguous")
        );
        fixture.store.close().await.unwrap();
    }
}

#[tokio::test]
async fn post_intent_survives_cancellation_and_restart_then_reuses_existing_tracker() {
    let fixture = Fixture::new().await;
    let shipment = fixture.shipment().await;
    let mut provider = Provider::new();
    let task = spawn(provider.worker(&fixture.store));
    provider.empty_lookup().await;
    let first = provider.request().await;
    assert_eq!(first.method, "POST");
    let pending = fixture.store.get(shipment.id).await.unwrap();
    let started = pending.sync.registration_started_at.unwrap();
    assert_eq!(pending.sync.registration_state, RegistrationState::Pending);
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    first.tracker();
    fixture.store.close().await.unwrap();
    let store = Store::open(&fixture.path).await.unwrap();
    sqlx::query("UPDATE shipments SET next_attempt_at=0,lease_until=0 WHERE id=?")
        .bind(shipment.id.to_string())
        .execute(store.pool())
        .await
        .unwrap();
    let task = spawn(provider.worker(&store));
    let lookup = provider.request().await;
    assert_eq!(lookup.method, "GET");
    let query = lookup.query();
    assert!(query.contains(&("tracking_codes[]".into(), NUMBER.into())));
    assert!(query.contains(&("carrier".into(), "USPS".into())));
    assert!(query.contains(&("page_size".into(), "100".into())));
    let start = query
        .iter()
        .find(|(key, _)| key == "start_datetime")
        .unwrap();
    let start =
        time::OffsetDateTime::parse(&start.1, &time::format_description::well_known::Rfc3339)
            .unwrap()
            .unix_timestamp();
    assert!((89 * 86400..=92 * 86400).contains(&(started - start)));
    lookup.page(vec![tracker(NUMBER)], false);
    assert!(task.await.unwrap().unwrap());
    let registered = store.get(shipment.id).await.unwrap();
    assert_eq!(
        registered.sync.registration_state,
        RegistrationState::Registered
    );
    assert_eq!(
        registered.sync.tracker_id.as_deref(),
        Some("trk_registration")
    );
    assert_eq!(registered.sync.registration_started_at, Some(started));
    assert!(registered.sync.last_error.is_none());
    provider.no_request();
    store.close().await.unwrap();
}

#[tokio::test]
async fn unknown_post_outcome_is_durable_and_empty_reconciliation_retries_same_identity() {
    let fixture = Fixture::new().await;
    let shipment = fixture.shipment().await;
    let mut provider = Provider::new();
    let task = spawn(provider.worker(&fixture.store));
    provider.empty_lookup().await;
    let first = provider.request().await;
    assert_eq!(first.method, "POST");
    let original: RegistrationRequest = serde_json::from_slice(&first.body).unwrap();
    first.respond(503, "private upstream details".into());
    assert!(task.await.unwrap().unwrap());
    let pending = fixture.store.get(shipment.id).await.unwrap();
    assert_eq!(pending.sync.registration_state, RegistrationState::Pending);
    assert_eq!(pending.sync.error_code.as_deref(), Some("provider_http"));
    assert_eq!(pending.sync.provider_status, Some(503));
    assert!(
        !pending
            .sync
            .last_error
            .as_ref()
            .unwrap()
            .contains("private")
    );
    assert!(pending.sync.next_attempt_at.is_some());
    let started = pending.sync.registration_started_at.unwrap();
    fixture.due(shipment.id).await;
    let task = spawn(provider.worker(&fixture.store));
    let lookup = provider.request().await;
    assert_eq!(lookup.method, "GET");
    lookup.page(Vec::new(), false);
    let repeated = provider.request().await;
    assert_eq!(repeated.method, "POST");
    let retry: RegistrationRequest = serde_json::from_slice(&repeated.body).unwrap();
    assert_eq!(retry.tracker, original.tracker);
    repeated.tracker();
    assert!(task.await.unwrap().unwrap());
    let registered = fixture.store.get(shipment.id).await.unwrap();
    assert_eq!(
        registered.sync.registration_state,
        RegistrationState::Registered
    );
    assert_eq!(registered.sync.registration_started_at, Some(started));
    assert!(registered.sync.error_code.is_none());
    assert!(registered.sync.provider_status.is_none());
    fixture.store.close().await.unwrap();
}

#[tokio::test]
async fn unsupported_provider_preserves_tracking_number_and_explains_support_without_retrying() {
    let fixture = Fixture::new().await;
    let shipment = fixture.shipment().await;
    let mut provider = Provider::new();
    let worker = provider.worker(&fixture.store);
    let task = spawn(worker.clone());
    provider.empty_lookup().await;
    provider.request().await.respond(
        422,
        r#"{"error":{"code":"TRACKER.NO_CARRIER_FOR_CODE","message":"private upstream details"}}"#
            .into(),
    );
    assert!(task.await.unwrap().unwrap());
    let rejected = fixture.store.get(shipment.id).await.unwrap();
    assert_eq!(
        rejected.sync.registration_state,
        RegistrationState::Rejected
    );
    assert_eq!(
        rejected.sync.error_code.as_deref(),
        Some("tracking_unsupported")
    );
    assert_eq!(rejected.sync.provider_status, Some(422));
    assert_eq!(
        rejected.sync.provider_error_code.as_deref(),
        Some("TRACKER.NO_CARRIER_FOR_CODE")
    );
    assert_eq!(rejected.tracking, shipment.tracking);
    assert!(rejected.sync.next_attempt_at.is_none());
    assert_eq!(
        rejected.sync.last_error.as_deref(),
        Some(
            "EasyPost could not match this tracking number to an available carrier. Select the carrier shown on the shipping notice and confirm its carrier account is configured in EasyPost. This does not mean the tracking number is invalid."
        )
    );
    fixture.due(shipment.id).await;
    assert!(!worker.tick().await.unwrap());
    provider.no_request();
    fixture.store.close().await.unwrap();
}

#[tokio::test]
async fn key_and_carrier_account_configuration_errors_preserve_parcels_and_pause_retries() {
    for (status, body, code) in [
        (401, "private key details", "tracking_provider_credentials"),
        (
            422,
            r#"{"error":{"code":"TRACKER.INVALID_TEST_CODE"}}"#,
            "tracking_test_key",
        ),
        (
            400,
            r#"{"error":{"code":"TRACKER.NO_CARRIER_ACCOUNT"}}"#,
            "tracking_carrier_account_required",
        ),
    ] {
        let fixture = Fixture::new().await;
        let shipment = fixture.shipment().await;
        let mut provider = Provider::new();
        let task = spawn(provider.worker(&fixture.store));
        let lookup = provider.request().await;
        assert_eq!(lookup.method, "GET");
        lookup.respond(status, body.into());
        assert!(task.await.unwrap().unwrap());
        let pending = fixture.store.get(shipment.id).await.unwrap();
        assert_eq!(
            pending.sync.registration_state,
            RegistrationState::ConfigurationRequired
        );
        assert_eq!(pending.sync.provider_status, Some(status));
        assert_eq!(pending.sync.error_code.as_deref(), Some(code));
        assert!(pending.sync.next_attempt_at.is_none());
        assert!(pending.sync.registration_started_at.is_some());
        fixture.store.close().await.unwrap();
    }
}

#[tokio::test]
async fn ambiguous_or_expired_outcomes_never_create_another_tracker() {
    for case in ["multiple", "incomplete", "expired", "wrong-code"] {
        let fixture = Fixture::new().await;
        let shipment = fixture.shipment().await;
        let age = if case == "expired" { 365 * 86400 } else { 60 };
        sqlx::query("UPDATE shipments SET registration_started_at=unixepoch()-? WHERE id=?")
            .bind(age)
            .bind(shipment.id.to_string())
            .execute(fixture.store.pool())
            .await
            .unwrap();
        let mut provider = Provider::new();
        let task = spawn(provider.worker(&fixture.store));
        let lookup = provider.request().await;
        assert_eq!(lookup.method, "GET");
        match case {
            "multiple" => lookup.page(vec![tracker(NUMBER), tracker(NUMBER)], false),
            "incomplete" => lookup.page(vec![tracker(NUMBER)], true),
            "wrong-code" => lookup.page(vec![tracker(REPLACEMENT)], false),
            _ => lookup.page(Vec::new(), false),
        }
        assert!(task.await.unwrap().unwrap());
        provider.no_request();
        let result = fixture.store.get(shipment.id).await.unwrap();
        assert!(result.sync.tracker_id.is_none());
        assert_eq!(
            result.sync.registration_state,
            if case == "wrong-code" {
                RegistrationState::Pending
            } else {
                RegistrationState::Rejected
            }
        );
        assert_eq!(
            result.sync.error_code.as_deref(),
            Some(match case {
                "expired" => "registration_expired",
                "wrong-code" => "provider_invalid_response",
                _ => "registration_ambiguous",
            })
        );
        fixture.store.close().await.unwrap();
    }
}

#[tokio::test]
async fn changed_tracking_during_reconciliation_cannot_post_the_old_identity() {
    let fixture = Fixture::new().await;
    let shipment = fixture.shipment().await;
    sqlx::query("UPDATE shipments SET registration_started_at=unixepoch()-60 WHERE id=?")
        .bind(shipment.id.to_string())
        .execute(fixture.store.pool())
        .await
        .unwrap();
    let mut provider = Provider::new();
    let task = spawn(provider.worker(&fixture.store));
    let lookup = provider.request().await;
    assert_eq!(lookup.method, "GET");
    let changed = fixture
        .store
        .update(
            shipment.id,
            shipment.revision,
            UpdateShipment {
                tracking: Patch {
                    value: Some(Some(InputTracking {
                        recipient_url: None,
                        number: REPLACEMENT.into(),
                        carrier: Some("USPS".parse().unwrap()),
                    })),
                },
                ..Default::default()
            },
            "test",
            None,
        )
        .await
        .unwrap();
    assert!(changed.sync.registration_started_at.is_none());
    lookup.page(Vec::new(), false);
    assert!(task.await.unwrap().unwrap());
    provider.no_request();
    let current = fixture.store.get(shipment.id).await.unwrap();
    assert_eq!(current.tracking.number, REPLACEMENT);
    assert!(current.sync.registration_started_at.is_none());
    assert_eq!(current.sync.registration_state, RegistrationState::Pending);
    fixture.store.close().await.unwrap();
}

#[tokio::test]
async fn a_real_request_timeout_preserves_the_parcel_intent_and_retry_state() {
    let fixture = Fixture::new().await;
    let shipment = fixture.shipment().await;
    let mut provider = Provider::new();
    let worker = provider.worker(&fixture.store);
    let task = spawn(worker.clone());
    provider.empty_lookup().await;
    let request = provider.request().await;
    assert_eq!(request.method, "POST");
    let before = fixture.store.get(shipment.id).await.unwrap();
    assert!(before.sync.registration_started_at.is_some());
    // The real HTTP client is waiting for this server. Advance its own timeout,
    // then resume the clock before SQLite's background threads complete writes.
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(21)).await;
    tokio::time::resume();
    assert!(task.await.unwrap().unwrap());
    drop(request);
    let pending = fixture.store.get(shipment.id).await.unwrap();
    assert_eq!(pending.title, shipment.title);
    assert_eq!(pending.tracking, shipment.tracking);
    assert_eq!(pending.direction, shipment.direction);
    assert_eq!(pending.sync.registration_state, RegistrationState::Pending);
    assert_eq!(pending.sync.error_code.as_deref(), Some("provider_timeout"));
    assert_eq!(pending.sync.provider_status, None);
    assert_eq!(
        pending.sync.registration_started_at,
        before.sync.registration_started_at
    );
    assert_eq!(pending.sync.attempts, 1);
    let remaining: i64 =
        sqlx::query_scalar("SELECT next_attempt_at-unixepoch() FROM shipments WHERE id=?")
            .bind(shipment.id.to_string())
            .fetch_one(fixture.store.pool())
            .await
            .unwrap();
    assert!((59..=60).contains(&remaining));
    assert!(!worker.tick().await.unwrap());
    provider.no_request();
    fixture.store.close().await.unwrap();
}

#[tokio::test]
async fn terminal_correction_during_outbound_pacing_cancels_registration() {
    let fixture = Fixture::new().await;
    let shipment = fixture.shipment().await;
    sqlx::query("UPDATE shipments SET registration_started_at=unixepoch()-60 WHERE id=?")
        .bind(shipment.id.to_string())
        .execute(fixture.store.pool())
        .await
        .unwrap();
    let mut provider = Provider::new();
    let client = EasyPostClient::for_loopback("registration-test-key", &provider.base).unwrap();
    let worker = SyncWorker::new(fixture.store.pool().clone(), Arc::new(client.clone()));
    let mut task = spawn(worker.clone());
    let lookup = provider.request().await;
    // Queue a read-only health request before the lookup response releases
    // its slot. Its gated response holds the FIFO slot while the worker waits
    // to prepare registration, without relying on artificial one-second gaps.
    let health = tokio::spawn(async move { client.check_connection().await });
    tokio::task::yield_now().await;
    lookup.page(Vec::new(), false);
    let held_health = provider.request().await;
    assert_eq!(held_health.method, "GET");
    let corrected = fixture
        .store
        .set_override(
            shipment.id,
            shipment.revision,
            Some(shipments::models::SetOverride {
                status: Status::Delivered,
                reason: "Received during registration".into(),
            }),
            "owner",
        )
        .await
        .unwrap();
    assert!(corrected.sync.next_attempt_at.is_none());
    held_health.page(Vec::new(), false);
    health.await.unwrap().unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        tokio::select! {
            result = &mut task => assert!(result.unwrap().unwrap()),
            request = provider.incoming.recv() => {
                let request = request.unwrap();
                panic!("obsolete registration sent {} {}", request.method, request.target);
            }
        }
    })
    .await
    .unwrap();
    provider.no_request();
    assert!(!worker.tick().await.unwrap());
    let current = fixture.store.get(shipment.id).await.unwrap();
    assert_eq!(current.status, Status::Delivered);
    assert!(current.sync.tracker_id.is_none());
    assert!(current.sync.next_attempt_at.is_none());
    let leases: i64 =
        sqlx::query_scalar("SELECT count(*) FROM shipments WHERE id=? AND lease_token IS NOT NULL")
            .bind(shipment.id.to_string())
            .fetch_one(fixture.store.pool())
            .await
            .unwrap();
    assert_eq!(leases, 0);
    fixture.store.close().await.unwrap();
}

#[tokio::test]
async fn terminal_correction_cancels_a_reconciled_post_and_releases_its_lease_for_explicit_refresh()
{
    use shipments::models::SetOverride;
    let fixture = Fixture::new().await;
    let shipment = fixture.shipment().await;
    sqlx::query("UPDATE shipments SET registration_started_at=unixepoch()-60 WHERE id=?")
        .bind(shipment.id.to_string())
        .execute(fixture.store.pool())
        .await
        .unwrap();
    let mut provider = Provider::new();
    let worker = provider.worker(&fixture.store);
    let task = spawn(worker.clone());
    let lookup = provider.request().await;
    assert_eq!(lookup.method, "GET");
    let corrected = fixture
        .store
        .set_override(
            shipment.id,
            shipment.revision,
            Some(SetOverride {
                status: Status::Delivered,
                reason: "Received locally".into(),
            }),
            "owner",
        )
        .await
        .unwrap();
    assert!(corrected.sync.next_attempt_at.is_none());
    lookup.page(Vec::new(), false);
    assert!(task.await.unwrap().unwrap());
    provider.no_request();
    assert!(!worker.tick().await.unwrap());
    let leases: i64 =
        sqlx::query_scalar("SELECT count(*) FROM shipments WHERE id=? AND lease_token IS NOT NULL")
            .bind(shipment.id.to_string())
            .fetch_one(fixture.store.pool())
            .await
            .unwrap();
    assert_eq!(leases, 0);
    let queued = fixture
        .store
        .refresh(shipment.id, corrected.revision, "owner")
        .await
        .unwrap();
    assert!(
        queued.sync.next_attempt_at.is_some(),
        "released cancellation must allow an explicit refresh"
    );
    fixture.due(shipment.id).await;
    let task = spawn(provider.worker(&fixture.store));
    let lookup = provider.request().await;
    assert_eq!(lookup.method, "GET");
    lookup.page(Vec::new(), false);
    let request = provider.request().await;
    assert_eq!(request.method, "POST");
    request.tracker();
    assert!(task.await.unwrap().unwrap());
    let complete = fixture.store.get(shipment.id).await.unwrap();
    assert_eq!(complete.status, Status::Delivered);
    assert_eq!(
        complete.sync.registration_state,
        RegistrationState::Registered
    );
    assert!(complete.sync.next_attempt_at.is_none());
    fixture.store.close().await.unwrap();
}

#[tokio::test]
async fn legacy_fedex_configuration_block_is_cleared_by_a_verified_carrier_correction() {
    use shipments::api;
    let fixture = Fixture::new().await;
    let shipment = fixture
        .store
        .create(
            CreateShipment {
                direction: Direction::Inbound,
                title: "Legacy carrier route regression parcel".into(),
                merchant: None,
                order_reference: None,
                signature_required: None,
                source: None,
                tracking: InputTracking {
                    recipient_url: None,
                    number: NUMBER.into(),
                    carrier: Some("FedEx".parse().unwrap()),
                },
            },
            "owner",
            "legacy-fedex-routing",
            None,
        )
        .await
        .unwrap();
    let mut provider = Provider::new();
    let worker = provider.worker(&fixture.store);
    let task = spawn(worker.clone());
    provider.empty_lookup().await;
    let request = provider.request().await;
    assert_eq!(request.method, "POST");
    let submitted: RegistrationRequest = serde_json::from_slice(&request.body).unwrap();
    assert_eq!(submitted.tracker.carrier, "FedEx");
    request.respond(404, r#"{"error":{"code":"CREDENTIALS_NOT_FOUND","message":"private carrier account and tracking details"}}"#.into());
    assert!(task.await.unwrap().unwrap());
    let blocked = fixture.store.get(shipment.id).await.unwrap();
    assert_eq!(
        blocked.sync.registration_state,
        RegistrationState::ConfigurationRequired
    );
    assert_eq!(blocked.sync.provider_status, Some(404));
    assert_eq!(
        blocked.sync.provider_error_code.as_deref(),
        Some("CREDENTIALS_NOT_FOUND")
    );
    assert!(
        blocked
            .sync
            .last_error
            .as_ref()
            .unwrap()
            .contains("Configure this carrier in EasyPost")
    );
    assert!(
        !blocked
            .sync
            .last_error
            .as_ref()
            .unwrap()
            .contains("private")
    );
    assert_eq!(blocked.tracking.carrier.as_str(), "FedEx");
    assert!(blocked.sync.next_attempt_at.is_none());
    assert!(!worker.tick().await.unwrap());
    provider.no_request();
    let owner = "carrier-routing-owner-credential-at-least-32-bytes";
    let client = EasyPostClient::for_loopback("registration-test-key", &provider.base).unwrap();
    let server = api::start(
        "127.0.0.1:0".parse().unwrap(),
        Arc::new(api::Context::new(
            fixture.store.clone(),
            owner,
            None,
            Some(Arc::new(client)),
        )),
    )
    .unwrap();
    let endpoint = format!(
        "http://{}/v1/shipments/{}",
        server.local_addr(),
        shipment.id
    );
    let task = tokio::spawn(async move {
        reqwest::Client::new()
            .patch(endpoint)
            .bearer_auth(owner)
            .header("If-Match", format!("\"{}\"", blocked.revision))
            .json(&UpdateShipment {
                tracking: Patch {
                    value: Some(Some(InputTracking {
                        recipient_url: None,
                        number: NUMBER.into(),
                        carrier: Some("FedExDefault".parse().unwrap()),
                    })),
                },
                ..Default::default()
            })
            .send()
            .await
            .unwrap()
    });
    provider.empty_lookup().await;
    let request = provider.request().await;
    assert_eq!(request.method, "POST");
    let submitted: RegistrationRequest = serde_json::from_slice(&request.body).unwrap();
    assert_eq!(submitted.tracker.carrier, "FedExDefault");
    let mut accepted = tracker(NUMBER);
    accepted.carrier = "FedExDefault".into();
    accepted.status = Status::OutForDelivery;
    request.respond(201, serde_json::to_string(&accepted).unwrap());
    let response = task.await.unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let registered: Shipment = response.json().await.unwrap();
    assert_eq!(registered.id, shipment.id);
    assert_eq!(registered.status, Status::OutForDelivery);
    assert_eq!(registered.tracking.carrier.as_str(), "FedExDefault");
    assert_eq!(
        registered.sync.registration_state,
        RegistrationState::Registered
    );
    assert_eq!(
        registered.sync.tracker_id.as_deref(),
        Some("trk_registration")
    );
    assert!(registered.sync.last_error.is_none());
    provider.no_request();
    server.close().await.unwrap();
    fixture.store.close().await.unwrap();
}

#[tokio::test]
async fn unrecognized_error_codes_and_oversized_bodies_preserve_http_status_without_private_data() {
    for body in [
        r#"{"error":{"code":"PRIVATE_NATIVE_SENTINEL","message":"private account and tracking details"}}"#.to_owned(),
        r#"{"error":{"code":"CREDENTIALS_NOT_FOUND","message":""#.to_owned() + &"private".repeat(50_000) + "\"}}",
    ] {
        let fixture = Fixture::new().await;
        let shipment = fixture.shipment().await;
        let mut provider = Provider::new();
        let task = spawn(provider.worker(&fixture.store));
        let lookup = provider.request().await;
        assert_eq!(lookup.method, "GET");
        lookup.respond(404, body);
        assert!(task.await.unwrap().unwrap());
        let pending = fixture.store.get(shipment.id).await.unwrap();
        assert_eq!(pending.sync.registration_state, RegistrationState::Pending);
        assert_eq!(pending.sync.provider_status, Some(404));
        assert_eq!(pending.sync.error_code.as_deref(), Some("provider_http"));
        assert!(pending.sync.provider_error_code.is_none());
        assert_eq!(pending.sync.last_error.as_deref(), Some("EasyPost returned HTTP 404"));
        assert!(pending.sync.next_attempt_at.is_some());
        fixture.store.close().await.unwrap();
    }
}

#[tokio::test]
async fn ordinary_fedex_resolves_before_registration_and_replay_never_registers_twice() {
    for rejected in [false, true] {
        let fixture = Fixture::new().await;
        let mut provider = Provider::new();
        let client = EasyPostClient::for_loopback("registration-test-key", &provider.base).unwrap();
        let owner = "carrier-default-owner-credential-at-least-32-bytes";
        let server = shipments::api::start(
            "127.0.0.1:0".parse().unwrap(),
            Arc::new(shipments::api::Context::new(
                fixture.store.clone(),
                owner,
                None,
                Some(Arc::new(client)),
            )),
        )
        .unwrap();
        let endpoint = format!("http://{}/v1/shipments", server.local_addr());
        let body = serde_json::json!({"direction":"inbound","title":"Synthetic default route parcel", "tracking":{"number":NUMBER,"carrier":"fedex"}});
        let send = |body: serde_json::Value| {
            let request = reqwest::Client::new()
                .post(&endpoint)
                .bearer_auth(owner)
                .header("Idempotency-Key", "default-route-test")
                .json(&body);
            tokio::spawn(async move { request.send().await.unwrap() })
        };
        let task = send(body.clone());
        provider.empty_lookup().await;
        let request = provider.request().await;
        let submitted: RegistrationRequest = serde_json::from_slice(&request.body).unwrap();
        assert_eq!(submitted.tracker.carrier, "FedExDefault");
        if rejected {
            request.respond(404, r#"{"error":{"code":"CREDENTIALS_NOT_FOUND"}}"#.into());
        } else {
            let mut accepted = tracker(NUMBER);
            accepted.carrier = "FedExDefault".into();
            request.respond(201, serde_json::to_string(&accepted).unwrap());
        }
        let response = task.await.unwrap();
        assert_eq!(response.status().as_u16(), if rejected { 400 } else { 201 });
        // Alias resolution happens before the idempotency fingerprint as well.
        let mut alias = body;
        alias["tracking"]["carrier"] = serde_json::json!("FedExDefault");
        let replay = send(alias).await.unwrap();
        assert_eq!(replay.status().as_u16(), if rejected { 400 } else { 201 });
        provider.no_request();
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM shipments")
            .fetch_one(fixture.store.pool())
            .await
            .unwrap();
        assert_eq!(count, if rejected { 0 } else { 1 });
        server.close().await.unwrap();
        fixture.store.close().await.unwrap();
    }
}
