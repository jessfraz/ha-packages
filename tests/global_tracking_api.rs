use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU16, AtomicUsize, Ordering},
        mpsc,
    },
    thread,
    time::Duration,
};

use dropshot::HttpServer;
use packages::{
    api,
    carrier::Carrier,
    models::{CreateShipment, Direction, InputTracking, Patch, Shipment, Status, UpdateShipment},
    shipping_api::{aftership::AfterShipClient, easypost::EasyPostClient, types::ProviderId},
    store::Store,
};
use reqwest::{Client, Response, StatusCode};
use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;
use uuid::Uuid;

const OWNER: &str = "global-tracking-disposable-owner-credential";
const NUMBER: &str = "GLOBAL123456";

#[derive(Default)]
struct Calls {
    total: AtomicUsize,
    registrations: AtomicUsize,
}

struct Provider {
    base: String,
    calls: Arc<Calls>,
    registration_status: Arc<AtomicU16>,
    started: Option<oneshot::Receiver<()>>,
    release: Option<mpsc::Sender<()>>,
    hold_registration: Arc<AtomicBool>,
    registration_started: Option<oneshot::Receiver<()>>,
    registration_release: Option<mpsc::Sender<()>>,
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl Provider {
    fn new(id: ProviderId, hold_first_response: bool) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}/", listener.local_addr().unwrap());
        let calls = Arc::new(Calls::default());
        let registration_status = Arc::new(AtomicU16::new(200));
        let response_status = registration_status.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let observed = calls.clone();
        let stopping = stop.clone();
        let (started, incoming) = oneshot::channel();
        let (release, released) = mpsc::channel();
        let hold_registration = Arc::new(AtomicBool::new(false));
        let holding_registration = hold_registration.clone();
        let (registration_started, registration_incoming) = oneshot::channel();
        let (registration_release, registration_released) = mpsc::channel();
        let thread = thread::spawn(move || {
            let mut started = Some(started);
            let mut registration_started = Some(registration_started);
            for stream in listener.incoming() {
                if stopping.load(Ordering::Acquire) {
                    break;
                }
                let Some((mut stream, method, target, body)) = read_request(stream.unwrap()) else {
                    continue;
                };
                observed.total.fetch_add(1, Ordering::SeqCst);
                if let Some(started) = started.take() {
                    let _ = started.send(());
                    if hold_first_response {
                        while !stopping.load(Ordering::Acquire) {
                            match released.recv_timeout(Duration::from_millis(20)) {
                                Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                                Err(mpsc::RecvTimeoutError::Timeout) => {}
                            }
                        }
                    }
                }
                if stopping.load(Ordering::Acquire) {
                    break;
                }
                let path = target.split('?').next().unwrap();
                if method == "POST" && matches!(path, "/trackers" | "/trackings") {
                    observed.registrations.fetch_add(1, Ordering::SeqCst);
                    if holding_registration.load(Ordering::Acquire) {
                        let _ = registration_started.take().unwrap().send(());
                        while !stopping.load(Ordering::Acquire) {
                            match registration_released.recv_timeout(Duration::from_millis(20)) {
                                Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                                Err(mpsc::RecvTimeoutError::Timeout) => {}
                            }
                        }
                    }
                }
                if stopping.load(Ordering::Acquire) {
                    break;
                }
                let status = if method == "POST" && path == "/trackers" {
                    response_status.swap(200, Ordering::SeqCst)
                } else {
                    200
                };
                let body = if status == 200 {
                    provider_response(id, &method, path, &body)
                } else {
                    // Real EasyPost error envelope, including a misleading
                    // rejection code on 503 to exercise uncertain outcomes.
                    r#"{"error":{"code":"TRACKER.NO_CARRIER_FOR_CODE","message":"No carrier found"}}"#.into()
                };
                let _ = write!(
                    stream,
                    "HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
            }
        });
        Self {
            base,
            calls,
            registration_status,
            started: Some(incoming),
            release: Some(release),
            hold_registration,
            registration_started: Some(registration_incoming),
            registration_release: Some(registration_release),
            stop,
            thread: Some(thread),
        }
    }

    async fn wait_started(&mut self) {
        tokio::time::timeout(Duration::from_secs(5), self.started.take().unwrap())
            .await
            .unwrap()
            .unwrap();
    }

    fn release(&mut self) {
        self.release.take().unwrap().send(()).unwrap();
    }

    fn calls(&self) -> usize {
        self.calls.total.load(Ordering::SeqCst)
    }

    fn registrations(&self) -> usize {
        self.calls.registrations.load(Ordering::SeqCst)
    }

    async fn wait_registration(&mut self) {
        tokio::time::timeout(
            Duration::from_secs(5),
            self.registration_started.take().unwrap(),
        )
        .await
        .unwrap()
        .unwrap();
    }
}

impl Drop for Provider {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.release.take();
        self.registration_release.take();
        let address = reqwest::Url::parse(&self.base)
            .unwrap()
            .socket_addrs(|| None)
            .unwrap()[0];
        let _ = TcpStream::connect(address);
        if let Some(thread) = self.thread.take() {
            thread.join().unwrap();
        }
    }
}

fn read_request(mut stream: TcpStream) -> Option<(TcpStream, String, String, Vec<u8>)> {
    stream.set_nonblocking(false).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
    let mut headers = Vec::new();
    while !headers.ends_with(b"\r\n\r\n") {
        let mut byte = [0];
        stream.read_exact(&mut byte).ok()?;
        headers.push(byte[0]);
        if headers.len() > 16 * 1024 {
            return None;
        }
    }
    let headers = String::from_utf8(headers).ok()?;
    let mut request = headers.lines().next()?.split_whitespace();
    let method = request.next()?.into();
    let target = request.next()?.into();
    let length = headers
        .lines()
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .map_or(Some(0), |(_, value)| value.trim().parse::<usize>().ok())?;
    let mut body = vec![0; length];
    stream.read_exact(&mut body).ok()?;
    Some((stream, method, target, body))
}

#[derive(Deserialize)]
struct EasyPostRequest {
    tracker: EasyPostTracking,
}

#[derive(Deserialize)]
struct EasyPostTracking {
    tracking_code: String,
    carrier: Option<Carrier>,
}

#[derive(Serialize)]
struct EasyPostReply {
    id: String,
    tracking_code: String,
    carrier: Carrier,
    status: Status,
}

#[derive(Serialize)]
struct EasyPostPage {
    trackers: Vec<EasyPostReply>,
    has_more: bool,
}

#[derive(Serialize)]
struct AfterShipEnvelope<T> {
    meta: AfterShipMeta,
    data: T,
}

#[derive(Serialize)]
struct AfterShipMeta {
    code: u16,
}

#[derive(Serialize)]
struct AfterShipPage {
    pagination: AfterShipPagination,
    trackings: Vec<AfterShipReply>,
}

#[derive(Serialize)]
struct AfterShipPagination {
    total: usize,
    next_cursor: Option<String>,
    has_next_page: bool,
}

#[derive(Deserialize)]
struct AfterShipTracking {
    tracking_number: String,
    slug: String,
}

#[derive(Deserialize)]
struct AfterShipDetection {
    tracking_number: String,
    slug: Vec<String>,
}

#[derive(Serialize)]
struct AfterShipReply {
    id: String,
    tracking_number: String,
    slug: String,
    tag: &'static str,
    subtag: &'static str,
}

#[derive(Serialize)]
struct DetectedCouriers {
    total: usize,
    couriers: Vec<Courier>,
}

#[derive(Serialize)]
struct Courier {
    slug: &'static str,
}

fn aftership<T: Serialize>(data: T) -> String {
    serde_json::to_string(&AfterShipEnvelope {
        meta: AfterShipMeta { code: 200 },
        data,
    })
    .unwrap()
}

fn provider_response(provider: ProviderId, method: &str, path: &str, body: &[u8]) -> String {
    match (provider, method, path) {
        (ProviderId::EasyPost, "GET", "/trackers") => serde_json::to_string(&EasyPostPage {
            trackers: Vec::new(),
            has_more: false,
        })
        .unwrap(),
        (ProviderId::EasyPost, "POST", "/trackers") => {
            let request: EasyPostRequest = serde_json::from_slice(body).unwrap();
            serde_json::to_string(&EasyPostReply {
                id: format!("trk_{}", request.tracker.tracking_code),
                tracking_code: request.tracker.tracking_code,
                carrier: request.tracker.carrier.unwrap_or(Carrier::Usps),
                status: Status::PreTransit,
            })
            .unwrap()
        }
        (ProviderId::AfterShip, "GET", "/trackings") => aftership(AfterShipPage {
            pagination: AfterShipPagination {
                total: 0,
                next_cursor: None,
                has_next_page: false,
            },
            trackings: Vec::new(),
        }),
        (ProviderId::AfterShip, "POST", "/couriers/detect") => {
            let request: AfterShipDetection = serde_json::from_slice(body).unwrap();
            let slug = if request.slug.len() == 1 {
                match request.slug[0].as_str() {
                    "speedx" => Some("speedx"),
                    "amazon" => Some("amazon"),
                    "sf-express" => Some("sf-express"),
                    other => panic!("Unexpected AfterShip courier: {other}"),
                }
            } else {
                assert!(request.slug.iter().any(|slug| slug == "amazon"));
                assert!(request.slug.iter().any(|slug| slug == "speedx"));
                match request.tracking_number.as_str() {
                    "TBA000000000001" | "TBA000000000002" => Some("amazon"),
                    "DISCOVERY000001" => Some("speedx"),
                    _ => None,
                }
            };
            aftership(DetectedCouriers {
                total: usize::from(slug.is_some()),
                couriers: slug.into_iter().map(|slug| Courier { slug }).collect(),
            })
        }
        (ProviderId::AfterShip, "POST", "/trackings") => {
            let request: AfterShipTracking = serde_json::from_slice(body).unwrap();
            aftership(AfterShipReply {
                id: format!("as_{}", request.tracking_number),
                tracking_number: request.tracking_number,
                slug: request.slug,
                tag: "InfoReceived",
                subtag: "InfoReceived_001",
            })
        }
        _ => panic!("Unexpected local provider request: {provider:?} {method} {path}"),
    }
}

struct App {
    _directory: tempfile::TempDir,
    store: Store,
    server: HttpServer<Arc<api::Context>>,
    client: Client,
    url: String,
    easypost: Provider,
    aftership: Provider,
}

impl App {
    async fn new(hold_easypost: bool) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(&directory.path().join("tracking.sqlite3"))
            .await
            .unwrap();
        let easypost = Provider::new(ProviderId::EasyPost, hold_easypost);
        let aftership = Provider::new(ProviderId::AfterShip, false);
        let context = api::Context::new(
            store.clone(),
            OWNER,
            None,
            Some(Arc::new(
                EasyPostClient::for_loopback("local-easypost-key", &easypost.base).unwrap(),
            )),
        )
        .with_provider(Arc::new(
            AfterShipClient::for_loopback("local-aftership-key", &aftership.base).unwrap(),
        ));
        let server = api::start("127.0.0.1:0".parse().unwrap(), Arc::new(context)).unwrap();
        let url = format!("http://{}", server.local_addr());
        Self {
            _directory: directory,
            store,
            server,
            client: Client::builder()
                .timeout(Duration::from_secs(5))
                .build()
                .unwrap(),
            url,
            easypost,
            aftership,
        }
    }

    fn create(&self, input: &CreateShipment, key: &str) -> reqwest::RequestBuilder {
        self.client
            .post(format!("{}/v1/packages", self.url))
            .bearer_auth(OWNER)
            .header("Idempotency-Key", key)
            .json(input)
    }

    fn patch(&self, original: &Shipment, tracking: InputTracking) -> reqwest::RequestBuilder {
        self.client
            .patch(format!("{}/v1/packages/{}", self.url, original.id))
            .bearer_auth(OWNER)
            .header("If-Match", format!("\"{}\"", original.revision))
            .json(&UpdateShipment {
                title: Some("Updated parcel".into()),
                tracking: Patch {
                    value: Some(Some(tracking)),
                },
                ..Default::default()
            })
    }

    async fn seed(&self, number: &str, archived: bool) -> Shipment {
        let shipment = self
            .store
            .create(
                input(number, Some(Carrier::Usps), Direction::Inbound),
                "owner",
                number,
                None,
            )
            .await
            .unwrap();
        if archived {
            self.store
                .archive(shipment.id, shipment.revision, true, "owner")
                .await
                .unwrap()
        } else {
            shipment
        }
    }

    fn calls(&self) -> (usize, usize) {
        (self.easypost.calls(), self.aftership.calls())
    }

    async fn count(&self) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM shipments")
            .fetch_one(self.store.pool())
            .await
            .unwrap()
    }

    async fn close(self) {
        self.server.close().await.unwrap();
        self.store.close().await.unwrap();
    }
}

fn input(number: &str, carrier: Option<Carrier>, direction: Direction) -> CreateShipment {
    CreateShipment {
        title: "Global tracking test parcel".into(),
        direction,
        merchant: None,
        order_reference: None,
        signature_required: None,
        tracking: InputTracking {
            number: number.into(),
            carrier,
            recipient_url: None,
        },
        source: None,
    }
}

#[derive(Deserialize)]
struct Failure {
    error_code: Option<String>,
    message: String,
}

async fn duplicate(response: Response, existing: Uuid) {
    assert_eq!(response.status(), StatusCode::CONFLICT);
    let failure: Failure = response.json().await.unwrap();
    assert_eq!(failure.error_code.as_deref(), Some("tracking_duplicate"));
    assert!(failure.message.contains(&existing.to_string()));
}

#[tokio::test]
async fn sf_express_uses_aftership_and_replays_without_duplicate_registration() {
    let app = App::new(false).await;
    let input = input(
        "SF0000000000001",
        Some(Carrier::SfExpress),
        Direction::Inbound,
    );
    let response = app.create(&input, "sf-express").send().await.unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let shipment: Shipment = response.json().await.unwrap();
    assert_eq!(shipment.sync.provider, ProviderId::AfterShip);
    assert_eq!(shipment.tracking.carrier, Carrier::SfExpress);
    assert_eq!(shipment.status, Status::PreTransit);
    assert_eq!(app.easypost.calls(), 0);
    assert_eq!(app.aftership.registrations(), 1);

    let calls = app.calls();
    let replay = app.create(&input, "sf-express").send().await.unwrap();
    assert!(replay.status().is_success());
    let replay: Shipment = replay.json().await.unwrap();
    assert_eq!(replay.id, shipment.id);
    assert_eq!(app.calls(), calls);
    let saved = app.store.get(shipment.id).await.unwrap();
    assert_eq!(saved.sync.provider, ProviderId::AfterShip);
    assert_eq!(saved.sync.tracker_id, shipment.sync.tracker_id);
    app.close().await;
}

#[tokio::test]
async fn resubmitting_sf_express_tracking_moves_the_existing_record_to_aftership() {
    use packages::{
        models::{CarrierTracking, Source},
        shipping_api::types::Tracker,
    };

    let app = App::new(false).await;
    let mut input = input(
        "SF0000000000002",
        Some(Carrier::SfExpress),
        Direction::Inbound,
    );
    input.signature_required = Some(true);
    input.source = Some(Source {
        namespace: "test".into(),
        key: "existing-sf-source".into(),
    });
    let original = app
        .store
        .create(
            input.clone(),
            "owner",
            "old-sf",
            Some(&Tracker {
                provider: ProviderId::EasyPost,
                id: "trk_legacy_sf".into(),
                tracking_code: input.tracking.number.clone(),
                carrier: Carrier::SfExpress,
                status: Status::Unknown,
                public_url: None,
                carrier_tracking: CarrierTracking::default(),
            }),
        )
        .await
        .unwrap();
    assert_eq!(original.sync.provider, ProviderId::EasyPost);

    let response = app
        .client
        .patch(format!("{}/v1/packages/{}", app.url, original.id))
        .bearer_auth(OWNER)
        .header("If-Match", format!("\"{}\"", original.revision))
        .json(&UpdateShipment {
            tracking: Patch {
                value: Some(Some(input.tracking)),
            },
            ..Default::default()
        })
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let updated: Shipment = response.json().await.unwrap();
    assert_eq!(updated.id, original.id);
    assert_eq!(updated.tracking, original.tracking);
    assert_eq!(updated.title, original.title);
    assert_eq!(updated.signature_required, original.signature_required);
    assert_eq!(updated.sources, original.sources);
    assert_eq!(updated.sync.provider, ProviderId::AfterShip);
    assert_eq!(
        updated.sync.tracker_id.as_deref(),
        Some("as_SF0000000000002")
    );
    assert_eq!(app.easypost.calls(), 0);
    assert_eq!(app.aftership.registrations(), 1);
    assert_eq!(app.count().await, 1);
    app.close().await;
}

#[tokio::test]
async fn unchanged_tracking_edits_preserve_amazon_owned_carrier_parcels() {
    use packages::{models::CarrierTracking, shipping_api::types::Tracker};

    for carrier in [Carrier::Usps, Carrier::Ups] {
        let app = App::new(false).await;
        let input = input("AMAZONNATIVE0001", Some(carrier), Direction::Inbound);
        let original = app
            .store
            .create(
                input,
                "owner",
                "amazon-owned",
                Some(&Tracker {
                    provider: ProviderId::AmazonAccount,
                    id: "amazon_native_parcel".into(),
                    tracking_code: "AMAZONNATIVE0001".into(),
                    carrier,
                    status: Status::InTransit,
                    public_url: None,
                    carrier_tracking: CarrierTracking::default(),
                }),
            )
            .await
            .unwrap();
        let response = app
            .patch(&original, original.tracking.clone().into())
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let updated: Shipment = response.json().await.unwrap();
        assert_eq!(updated.title, "Updated parcel");
        assert_eq!(updated.sync.provider, ProviderId::AmazonAccount);
        assert_eq!(updated.sync.tracker_id, original.sync.tracker_id);
        assert_eq!(updated.tracking, original.tracking);
        assert_eq!(updated.status, original.status);
        assert_eq!(app.calls(), (0, 0));
        app.close().await;
    }
}

#[tokio::test]
async fn create_duplicates_ignore_carrier_provider_direction_and_archive_state() {
    for archived in [false, true] {
        let app = App::new(false).await;
        let existing = app.seed(NUMBER, archived).await;
        for (index, (number, carrier, direction)) in [
            (NUMBER, Some(Carrier::Ups), Direction::Inbound),
            (NUMBER, Some(Carrier::SpeedX), Direction::Inbound),
            (NUMBER, None, Direction::Inbound),
            (NUMBER, Some(Carrier::Usps), Direction::Outbound),
            (
                " global 123456 \u{a0}",
                Some(Carrier::Ups),
                Direction::Outbound,
            ),
        ]
        .into_iter()
        .enumerate()
        {
            duplicate(
                app.create(
                    &input(number, carrier, direction),
                    &format!("duplicate-{index}"),
                )
                .send()
                .await
                .unwrap(),
                existing.id,
            )
            .await;
            assert_eq!(app.calls(), (0, 0));
        }
        assert_eq!(app.count().await, 1);
        app.close().await;
    }
}

#[tokio::test]
async fn patch_duplicate_is_rejected_without_provider_calls_or_partial_changes() {
    for original_archived in [false, true] {
        for existing_archived in [false, true] {
            let app = App::new(false).await;
            let original = app.seed("ORIGINAL123", original_archived).await;
            let existing = app.seed(NUMBER, existing_archived).await;
            duplicate(
                app.patch(
                    &original,
                    input(
                        " global 123456 ",
                        Some(Carrier::SpeedX),
                        Direction::Outbound,
                    )
                    .tracking,
                )
                .send()
                .await
                .unwrap(),
                existing.id,
            )
            .await;
            let current = app.store.get(original.id).await.unwrap();
            assert_eq!(current.tracking, original.tracking);
            assert_eq!(current.title, original.title);
            assert_eq!(current.revision, original.revision);
            assert_eq!(current.archived, original.archived);
            assert_eq!(app.calls(), (0, 0));
            assert_eq!(app.count().await, 2);
            app.close().await;
        }
    }
}

#[tokio::test]
async fn same_shipment_can_change_carrier_including_its_tracking_provider() {
    for carrier in [Carrier::Ups, Carrier::SpeedX] {
        let app = App::new(false).await;
        let original = app.seed(NUMBER, false).await;
        let response = app
            .patch(
                &original,
                input(NUMBER, Some(carrier), Direction::Inbound).tracking,
            )
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let changed: Shipment = response.json().await.unwrap();
        assert_eq!(changed.id, original.id);
        assert_eq!(changed.tracking.number, NUMBER);
        assert_eq!(changed.tracking.carrier, carrier);
        assert_eq!(changed.revision, original.revision + 1);
        assert_eq!(
            changed.sync.provider,
            ProviderId::for_carrier(Some(carrier))
        );
        assert_eq!(app.count().await, 1);
        let registrations = (app.easypost.registrations(), app.aftership.registrations());
        assert_eq!(
            registrations,
            if carrier == Carrier::Ups {
                (1, 0)
            } else {
                (0, 1)
            }
        );
        app.close().await;
    }
}

#[tokio::test]
async fn idempotent_create_replays_unchanged_even_after_archiving() {
    let app = App::new(false).await;
    let body = input(NUMBER, Some(Carrier::Usps), Direction::Inbound);
    let response = app.create(&body, "replay").send().await.unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let original_body = response.bytes().await.unwrap();
    let original: Shipment = serde_json::from_slice(&original_body).unwrap();
    let calls = app.calls();
    assert_eq!(app.easypost.registrations(), 1);
    app.store
        .archive(original.id, original.revision, true, "owner")
        .await
        .unwrap();
    let response = app.create(&body, "replay").send().await.unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    assert_eq!(response.bytes().await.unwrap(), original_body);
    assert_eq!(app.calls(), calls);
    assert_eq!(app.count().await, 1);
    app.close().await;
}

#[tokio::test]
async fn pending_number_cannot_register_through_a_different_provider() {
    let mut app = App::new(true).await;
    let request = app.create(
        &input(NUMBER, Some(Carrier::Usps), Direction::Inbound),
        "race-easypost",
    );
    let first = tokio::spawn(async move { request.send().await.unwrap() });
    app.easypost.wait_started().await;
    let competing = input(NUMBER, Some(Carrier::SpeedX), Direction::Outbound);
    let response = app
        .create(&competing, "race-aftership")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(response.headers()["Retry-After"], "120");
    let failure: Failure = response.json().await.unwrap();
    assert_eq!(
        failure.error_code.as_deref(),
        Some("tracking_verification_pending")
    );
    assert_eq!(app.aftership.calls(), 0);
    assert_eq!(app.aftership.registrations(), 0);
    assert_eq!(app.count().await, 0);
    app.easypost.release();
    let response = first.await.unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let saved: Shipment = response.json().await.unwrap();
    duplicate(
        app.create(&competing, "race-aftership")
            .send()
            .await
            .unwrap(),
        saved.id,
    )
    .await;
    assert_eq!(app.aftership.calls(), 0);
    assert_eq!(app.easypost.registrations(), 1);
    assert_eq!(app.count().await, 1);
    app.close().await;
}

#[tokio::test]
async fn preparation_renews_live_leases_and_fences_lost_number_claims_before_registration() {
    for ownership in ["live", "expired", "stolen"] {
        let mut app = App::new(true).await;
        app.easypost
            .hold_registration
            .store(true, Ordering::Release);
        let request = app.create(
            &input(NUMBER, Some(Carrier::Usps), Direction::Inbound),
            "slow-preparation",
        );
        let pending = tokio::spawn(async move { request.send().await.unwrap() });
        app.easypost.wait_started().await;
        sqlx::query(
            "UPDATE admission_tracking SET lease_until=unixepoch()+30 WHERE tracking_number=?",
        )
        .bind(NUMBER)
        .execute(app.store.pool())
        .await
        .unwrap();
        sqlx::query("UPDATE tracking_verifications SET lease_until=unixepoch()+30 WHERE provider='easypost' AND tracking_number=?")
            .bind(NUMBER).execute(app.store.pool()).await.unwrap();
        match ownership {
            "expired" => {
                sqlx::query("UPDATE admission_tracking SET lease_until=unixepoch()-1 WHERE tracking_number=?")
                    .bind(NUMBER).execute(app.store.pool()).await.unwrap();
            }
            "stolen" => {
                sqlx::query("UPDATE admission_tracking SET lease_token='successor-test-token' WHERE tracking_number=?")
                    .bind(NUMBER).execute(app.store.pool()).await.unwrap();
            }
            _ => {}
        }
        app.easypost.release();
        if ownership == "live" {
            app.easypost.wait_registration().await;
            let remaining = sqlx::query_as::<_, (i64, i64)>(
                "SELECT (SELECT lease_until-unixepoch() FROM admission_tracking WHERE tracking_number=?), \
                 (SELECT lease_until-unixepoch() FROM tracking_verifications WHERE provider='easypost' AND tracking_number=?)",
            ).bind(NUMBER).bind(NUMBER).fetch_one(app.store.pool()).await.unwrap();
            assert!((115..=120).contains(&remaining.0));
            assert!((115..=120).contains(&remaining.1));
            assert_eq!(app.count().await, 0);
            app.easypost
                .registration_release
                .take()
                .unwrap()
                .send(())
                .unwrap();
            assert_eq!(pending.await.unwrap().status(), StatusCode::CREATED);
            assert_eq!(app.count().await, 1);
            assert_eq!(app.easypost.registrations(), 1);
        } else {
            assert_eq!(
                pending.await.unwrap().status(),
                StatusCode::SERVICE_UNAVAILABLE
            );
            assert_eq!(app.easypost.registrations(), 0);
            assert_eq!(app.count().await, 0);
            if ownership == "stolen" {
                let owner: String = sqlx::query_scalar(
                    "SELECT lease_token FROM admission_tracking WHERE tracking_number=?",
                )
                .bind(NUMBER)
                .fetch_one(app.store.pool())
                .await
                .unwrap();
                assert_eq!(owner, "successor-test-token");
            }
        }
        assert_eq!(app.aftership.calls(), 0);
        app.close().await;
    }
}

#[tokio::test]
async fn amazon_auto_tracking_uses_aftership_after_definite_rejection_and_replays_once() {
    let app = App::new(false).await;
    app.easypost
        .registration_status
        .store(400, Ordering::SeqCst);
    let input = input("TBA000000000001", None, Direction::Inbound);
    let response = app.create(&input, "amazon-auto").send().await.unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let saved: Shipment = response.json().await.unwrap();
    assert_eq!(saved.tracking.carrier, Carrier::AmazonMcf);
    assert_eq!(saved.sync.provider, ProviderId::AfterShip);
    assert_eq!(app.easypost.registrations(), 1);
    assert_eq!(app.aftership.registrations(), 1);
    assert_eq!(app.count().await, 1);

    let calls = app.calls();
    let replay: Shipment = app
        .create(&input, "amazon-auto")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(replay.id, saved.id);
    assert_eq!(app.calls(), calls);
    duplicate(
        app.create(&input, "another-amazon-request")
            .send()
            .await
            .unwrap(),
        saved.id,
    )
    .await;
    assert_eq!(app.calls(), calls);
    app.close().await;
}

#[tokio::test]
async fn amazon_auto_tracking_resolves_replacement_tracking_through_the_same_path() {
    let app = App::new(false).await;
    let original = app.seed(NUMBER, false).await;
    app.easypost
        .registration_status
        .store(400, Ordering::SeqCst);
    let response = app
        .patch(
            &original,
            input("TBA000000000002", None, Direction::Inbound).tracking,
        )
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let updated: Shipment = response.json().await.unwrap();
    assert_eq!(updated.id, original.id);
    assert_eq!(updated.tracking.carrier, Carrier::AmazonMcf);
    assert_eq!(updated.sync.provider, ProviderId::AfterShip);
    assert_eq!(app.aftership.registrations(), 1);
    assert_eq!(app.count().await, 1);
    app.close().await;
}

#[tokio::test]
async fn automatic_discovery_uses_the_same_trait_path_for_non_amazon_carriers() {
    let app = App::new(false).await;
    app.easypost
        .registration_status
        .store(400, Ordering::SeqCst);
    let response = app
        .create(
            &input("DISCOVERY000001", None, Direction::Inbound),
            "discovery",
        )
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let saved: Shipment = response.json().await.unwrap();
    assert_eq!(saved.tracking.carrier, Carrier::SpeedX);
    assert_eq!(saved.sync.provider, ProviderId::AfterShip);
    assert_eq!(app.aftership.registrations(), 1);
    app.close().await;
}

#[tokio::test]
async fn configured_provider_discovers_without_easypost_but_retains_its_uncertain_intents() {
    for previous_uncertain in [false, true] {
        let app = App::new(false).await;
        let input = input("TBA000000000001", None, Direction::Inbound);
        if previous_uncertain {
            app.easypost
                .registration_status
                .store(503, Ordering::SeqCst);
            assert_eq!(
                app.create(&input, "default-removed")
                    .send()
                    .await
                    .unwrap()
                    .status(),
                StatusCode::SERVICE_UNAVAILABLE,
            );
        }
        // A second real server consumes the same durable state after its
        // configuration removes EasyPost. No fake admission state is seeded.
        let context =
            api::Context::new(app.store.clone(), OWNER, None, None).with_provider(Arc::new(
                AfterShipClient::for_loopback("local-aftership-key", &app.aftership.base).unwrap(),
            ));
        let server = api::start("127.0.0.1:0".parse().unwrap(), Arc::new(context)).unwrap();
        let response = app
            .client
            .post(format!("http://{}/v1/packages", server.local_addr()))
            .bearer_auth(OWNER)
            .header("Idempotency-Key", "default-removed")
            .json(&input)
            .send()
            .await
            .unwrap();
        if previous_uncertain {
            assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
            let failure: Failure = response.json().await.unwrap();
            assert_eq!(
                failure.error_code.as_deref(),
                Some("tracking_provider_unconfigured")
            );
            assert_eq!(app.aftership.calls(), 0);
            assert_eq!(app.count().await, 0);
        } else {
            assert_eq!(response.status(), StatusCode::CREATED);
            let saved: Shipment = response.json().await.unwrap();
            assert_eq!(saved.tracking.carrier, Carrier::AmazonMcf);
            assert_eq!(saved.sync.provider, ProviderId::AfterShip);
            assert_eq!(app.aftership.registrations(), 1);
            assert_eq!(app.easypost.calls(), 0);
        }
        server.close().await.unwrap();
        app.close().await;
    }
}

#[tokio::test]
async fn automatic_retry_resumes_the_registered_provider_after_the_shipment_commit_fails() {
    let app = App::new(false).await;
    app.easypost
        .registration_status
        .store(400, Ordering::SeqCst);
    sqlx::query("CREATE TRIGGER fail_shipment_insert BEFORE INSERT ON shipments BEGIN SELECT RAISE(FAIL, 'injected shipment commit failure'); END")
        .execute(app.store.pool()).await.unwrap();
    let input = input("TBA000000000001", None, Direction::Inbound);
    assert_eq!(
        app.create(&input, "recover-provider")
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::INTERNAL_SERVER_ERROR,
    );
    assert_eq!(app.count().await, 0);
    assert_eq!(app.easypost.registrations(), 1);
    assert_eq!(app.aftership.registrations(), 1);
    sqlx::query("DROP TRIGGER fail_shipment_insert")
        .execute(app.store.pool())
        .await
        .unwrap();
    sqlx::query("UPDATE tracking_verifications SET next_attempt_at=0 WHERE provider='easypost'")
        .execute(app.store.pool())
        .await
        .unwrap();
    let calls = app.calls();
    let response = app.create(&input, "recover-provider").send().await.unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let saved: Shipment = response.json().await.unwrap();
    assert_eq!(saved.tracking.carrier, Carrier::AmazonMcf);
    assert_eq!(saved.sync.provider, ProviderId::AfterShip);
    assert_eq!(app.calls(), calls);
    assert_eq!(app.aftership.registrations(), 1);
    assert_eq!(app.count().await, 1);
    app.close().await;
}

#[tokio::test]
async fn amazon_auto_tracking_preserves_uncertainty_and_explicit_carriers() {
    for (status, carrier, number, expected) in [
        (
            503,
            None,
            "TBA000000000003",
            StatusCode::SERVICE_UNAVAILABLE,
        ),
        (
            400,
            Some(Carrier::Usps),
            "TBA000000000003",
            StatusCode::BAD_REQUEST,
        ),
        (400, None, "OTHER000000000003", StatusCode::BAD_REQUEST),
        (400, None, "TBA00000000000X", StatusCode::BAD_REQUEST),
    ] {
        let app = App::new(false).await;
        app.easypost
            .registration_status
            .store(status, Ordering::SeqCst);
        let input = input(number, carrier, Direction::Inbound);
        assert_eq!(
            app.create(&input, "rejected-or-uncertain")
                .send()
                .await
                .unwrap()
                .status(),
            expected,
        );
        let calls = app.calls();
        assert_eq!(
            app.create(&input, "rejected-or-uncertain")
                .send()
                .await
                .unwrap()
                .status(),
            expected,
        );
        assert_eq!(app.easypost.calls(), calls.0);
        let discovery_reads = usize::from(status == 400 && carrier.is_none());
        assert_eq!(app.aftership.calls(), discovery_reads * 2);
        assert_eq!(app.aftership.registrations(), 0);
        assert_eq!(app.easypost.registrations(), 1);
        assert_eq!(app.count().await, 0);
        app.close().await;
    }
}
