use shipments::shipping_api::easypost::EasyPostClient;
use shipments::shipping_api::types::Tracker;
use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::JoinHandle,
    time::Duration,
};

use dropshot::ResultsPage;
use reqwest::{Client, StatusCode};
use serde::Serialize;
use shipments::{
    api,
    models::{
        CarrierTracking, CreateShipment, DeliveryEstimate, Direction, InputTracking, Patch,
        SetOverride, Shipment, StateEventPage, StateEventType, Status, UpdateShipment,
    },
    store::Store,
    sync::SyncWorker,
};
use sqlx::{QueryBuilder, Row, Sqlite};

const OWNER: &str = "state-feed-owner-fixture-at-least-32-bytes";
const INTEGRATION: &str = "state-feed-integration-fixture-at-least-32-bytes";

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
                    title: "Example parcel".into(),
                    direction: Direction::Outbound,
                    merchant: None,
                    order_reference: None,
                    signature_required: None,
                    tracking: InputTracking {
                        recipient_url: None,
                        carrier: Some("usps".parse().unwrap()),
                        number: number.into(),
                    },
                    source: None,
                },
                "owner",
                number,
                None,
            )
            .await
            .unwrap()
    }

    async fn correct(&self, shipment: &Shipment, status: Status) -> Shipment {
        self.store
            .set_override(
                shipment.id,
                shipment.revision,
                Some(SetOverride {
                    status,
                    reason: "Example correction".into(),
                }),
                "owner",
            )
            .await
            .unwrap()
    }
}

#[tokio::test]
async fn transitions_between_polls_preserve_snapshots_without_metadata_or_archive_noise() {
    let fixture = Fixture::new().await;
    let checkpoint = fixture.store.state_events(None, 200).await.unwrap();
    assert_eq!(checkpoint.next_after, 0);
    let mut shipment = fixture.shipment("EVENTS001").await;
    assert!(
        fixture
            .store
            .state_events(Some(0), 200)
            .await
            .unwrap()
            .items
            .is_empty()
    );
    shipment = fixture.correct(&shipment, Status::OutForDelivery).await;
    let first_revision = shipment.revision;
    shipment = fixture
        .store
        .update(
            shipment.id,
            shipment.revision,
            UpdateShipment {
                title: Some("Example renamed parcel".into()),
                ..Default::default()
            },
            "owner",
            None,
        )
        .await
        .unwrap();
    shipment = fixture.correct(&shipment, Status::OutForDelivery).await;
    assert!(shipment.revision > first_revision);
    shipment = fixture.correct(&shipment, Status::Delivered).await;
    shipment = fixture
        .store
        .archive(shipment.id, shipment.revision, true, "owner")
        .await
        .unwrap();
    shipment = fixture
        .store
        .archive(shipment.id, shipment.revision, false, "owner")
        .await
        .unwrap();
    shipment = fixture.correct(&shipment, Status::Error).await;
    let page = fixture
        .store
        .state_events(Some(checkpoint.next_after), 200)
        .await
        .unwrap();
    assert_eq!(page.items.len(), 3);
    assert_eq!(
        page.items
            .iter()
            .map(|event| event.event_type)
            .collect::<Vec<_>>(),
        [
            StateEventType::OutForDelivery,
            StateEventType::Delivered,
            StateEventType::Exception
        ]
    );
    assert_eq!(
        page.items
            .iter()
            .map(|event| event.status)
            .collect::<Vec<_>>(),
        [Status::OutForDelivery, Status::Delivered, Status::Error]
    );
    assert_eq!(page.items[0].title, "Example parcel");
    assert_eq!(page.items[1].title, "Example renamed parcel");
    for event in &page.items {
        assert_eq!(event.shipment_id, shipment.id);
        assert_eq!(event.direction, Direction::Outbound);
        assert_eq!(event.carrier, Some("usps".parse().unwrap()));
        assert!(!event.at.is_empty());
    }
    assert!(
        page.items
            .windows(2)
            .all(|events| events[0].id < events[1].id)
    );
    assert_eq!(page.next_after, page.items.last().unwrap().id);
    assert!(!page.has_more);
    let unchanged = fixture
        .store
        .state_events(Some(page.next_after), 200)
        .await
        .unwrap();
    assert!(unchanged.items.is_empty());
    assert_eq!(unchanged.next_after, page.next_after);
    assert!(!unchanged.has_more);
    fixture.store.close().await.unwrap();
}

#[tokio::test]
async fn tail_bootstrap_bounded_pages_and_restart_keep_a_durable_checkpoint() {
    let mut fixture = Fixture::new().await;
    let shipment = fixture.shipment("EVENTS002").await;
    let shipment = fixture.correct(&shipment, Status::OutForDelivery).await;
    let bootstrap = fixture.store.state_events(None, 1).await.unwrap();
    assert!(bootstrap.items.is_empty());
    assert!(bootstrap.next_after > 0);
    assert!(!bootstrap.has_more);
    let backup = tempfile::tempdir().unwrap();
    let backup_path = backup.path().join("restored.sqlite3");
    fixture.store.close().await.unwrap();
    std::fs::copy(&fixture.path, &backup_path).unwrap();
    fixture.store = Store::open(&fixture.path).await.unwrap();
    let shipment = fixture.correct(&shipment, Status::Delivered).await;
    fixture.correct(&shipment, Status::Failure).await;
    let first = fixture
        .store
        .state_events(Some(bootstrap.next_after), 1)
        .await
        .unwrap();
    assert_eq!(first.items.len(), 1);
    assert_eq!(first.items[0].event_type, StateEventType::Delivered);
    assert!(first.has_more);
    fixture.store.close().await.unwrap();
    let restored = Store::open(&fixture.path).await.unwrap();
    let second = restored
        .state_events(Some(first.next_after), 1)
        .await
        .unwrap();
    assert_eq!(second.items.len(), 1);
    assert_eq!(second.items[0].event_type, StateEventType::Exception);
    assert!(second.next_after > first.next_after);
    assert!(!second.has_more);
    let replay = restored
        .state_events(Some(bootstrap.next_after), 1)
        .await
        .unwrap();
    assert_eq!(replay.items[0].id, first.items[0].id);
    assert_eq!(replay.next_after, first.next_after);
    let initial = restored.state_events(Some(0), 200).await.unwrap();
    assert_eq!(initial.items.len(), 3);
    assert_eq!(initial.next_after, second.next_after);
    assert!(restored.state_events(Some(-1), 10).await.is_err());
    assert!(restored.state_events(Some(0), 0).await.is_err());
    assert!(restored.state_events(Some(0), 201).await.is_err());
    restored.close().await.unwrap();
    let old_backup = Store::open(&backup_path).await.unwrap();
    assert!(matches!(
        old_backup.state_events(Some(second.next_after), 10).await,
        Err(shipments::error::Error::Conflict(_))
    ));
    let reset = old_backup.state_events(None, 10).await.unwrap();
    assert_eq!(reset.next_after, bootstrap.next_after);
    assert!(reset.items.is_empty());
    assert!(
        old_backup
            .state_events(Some(reset.next_after), 10)
            .await
            .unwrap()
            .items
            .is_empty()
    );
    old_backup.close().await.unwrap();
}

#[tokio::test]
async fn only_effective_notification_states_emit_events() {
    let fixture = Fixture::new().await;
    let mut shipment = fixture.shipment("EVENTS006").await;
    for status in [
        Status::Unknown,
        Status::PreTransit,
        Status::InTransit,
        Status::AvailableForPickup,
        Status::Cancelled,
    ] {
        shipment = fixture.correct(&shipment, status).await;
    }
    assert!(
        fixture
            .store
            .state_events(Some(0), 200)
            .await
            .unwrap()
            .items
            .is_empty()
    );
    let expected = [
        Status::OutForDelivery,
        Status::Delivered,
        Status::ReturnToSender,
        Status::Failure,
        Status::Error,
    ];
    for status in expected {
        shipment = fixture.correct(&shipment, status).await;
        shipment = fixture.correct(&shipment, status).await;
    }
    let page = fixture.store.state_events(Some(0), 200).await.unwrap();
    assert_eq!(
        page.items
            .iter()
            .map(|event| event.status)
            .collect::<Vec<_>>(),
        expected
    );
    assert!(
        page.items[2..]
            .iter()
            .all(|event| event.event_type == StateEventType::Exception)
    );
    fixture.store.close().await.unwrap();
}

#[tokio::test]
async fn notification_failure_rolls_back_the_shipment_source_event_and_feed_together() {
    let fixture = Fixture::new().await;
    let shipment = fixture.shipment("EVENTS003").await;
    let history = fixture.store.events(shipment.id, 0, 100).await.unwrap();
    sqlx::query("CREATE TRIGGER fixture_reject_notification AFTER INSERT ON state_events BEGIN SELECT RAISE(ABORT, 'fixture notification failure'); END")
        .execute(fixture.store.pool()).await.unwrap();
    assert!(
        fixture
            .store
            .set_override(
                shipment.id,
                shipment.revision,
                Some(SetOverride {
                    status: Status::Delivered,
                    reason: "Example correction".into(),
                }),
                "owner"
            )
            .await
            .is_err()
    );
    let unchanged = fixture.store.get(shipment.id).await.unwrap();
    assert_eq!(unchanged.revision, shipment.revision);
    assert_eq!(unchanged.status, Status::Expected);
    assert!(unchanged.status_override.is_none());
    assert_eq!(
        fixture
            .store
            .events(shipment.id, 0, 100)
            .await
            .unwrap()
            .len(),
        history.len()
    );
    assert!(
        fixture
            .store
            .state_events(Some(0), 200)
            .await
            .unwrap()
            .items
            .is_empty()
    );
    sqlx::query("DROP TRIGGER fixture_reject_notification")
        .execute(fixture.store.pool())
        .await
        .unwrap();
    fixture.correct(&shipment, Status::Delivered).await;
    let committed = fixture.store.state_events(Some(0), 200).await.unwrap();
    assert_eq!(committed.items.len(), 1);
    assert_eq!(committed.items[0].status, Status::Delivered);
    fixture.store.close().await.unwrap();
}

#[tokio::test]
async fn event_route_authenticates_and_keeps_the_same_cursor_contract() {
    let fixture = Fixture::new().await;
    let shipment = fixture.shipment("EVENTS004").await;
    fixture.correct(&shipment, Status::OutForDelivery).await;
    let server = api::start(
        "127.0.0.1:0".parse().unwrap(),
        Arc::new(
            api::Context::new(fixture.store.clone(), OWNER, None, None)
                .with_integration(INTEGRATION),
        ),
    )
    .unwrap();
    let origin = format!("http://{}", server.local_addr());
    let client = Client::new();
    assert_eq!(
        client
            .get(format!("{origin}/v1/events?after=0"))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    for token in [OWNER, INTEGRATION] {
        let response = client
            .get(format!("{origin}/v1/events?after=0&limit=1"))
            .bearer_auth(token)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["cache-control"], "no-store");
        let page: StateEventPage = response.json().await.unwrap();
        assert_eq!(page.items.len(), 1);
        assert_eq!(page.items[0].shipment_id, shipment.id);
        assert_eq!(page.items[0].id, page.next_after);
        assert!(!page.has_more);
    }
    let tail: StateEventPage = client
        .get(format!("{origin}/v1/events"))
        .bearer_auth(INTEGRATION)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(tail.items.is_empty());
    assert!(tail.next_after > 0);
    for query in ["after=-1", "limit=0", "limit=201"] {
        assert_eq!(
            client
                .get(format!("{origin}/v1/events?{query}"))
                .bearer_auth(INTEGRATION)
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::BAD_REQUEST
        );
    }
    assert_eq!(
        client
            .get(format!("{origin}/v1/events?after={}", i64::MAX))
            .bearer_auth(INTEGRATION)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::CONFLICT
    );
    server.close().await.unwrap();
    fixture.store.close().await.unwrap();
}

#[derive(Serialize)]
struct TrackerReply {
    id: &'static str,
    tracking_code: &'static str,
    carrier: &'static str,
    status: Status,
    est_delivery_date: &'static str,
}

struct Provider {
    url: String,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Provider {
    fn start(statuses: Vec<Status>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}/v2/", listener.local_addr().unwrap());
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        let thread = std::thread::spawn(move || {
            for (index, status) in statuses.into_iter().enumerate() {
                while !stopping.load(Ordering::Relaxed) {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            Self::reply(stream, status, index);
                            break;
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(2))
                        }
                        Err(error) => panic!("fixture accept: {error}"),
                    }
                }
                if stopping.load(Ordering::Relaxed) {
                    break;
                }
            }
        });
        Self {
            url,
            stop,
            thread: Some(thread),
        }
    }

    fn reply(mut stream: TcpStream, status: Status, index: usize) {
        stream.set_nonblocking(false).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut headers = Vec::new();
        while !headers.ends_with(b"\r\n\r\n") {
            let mut byte = [0];
            stream.read_exact(&mut byte).unwrap();
            headers.push(byte[0]);
            assert!(headers.len() <= 16 * 1024);
        }
        let headers = String::from_utf8(headers).unwrap();
        assert!(headers.starts_with(if index == 0 {
            "GET /v2/trackers?"
        } else {
            "GET /v2/trackers/trk_feed_fixture "
        }));
        if index == 0 {
            let path = headers.split_whitespace().nth(1).unwrap();
            let url = reqwest::Url::parse(&format!("http://provider{path}")).unwrap();
            let query: std::collections::HashMap<_, _> = url.query_pairs().collect();
            assert_eq!(
                query.get("tracking_codes[]").map(|value| value.as_ref()),
                Some("EVENTS005")
            );
            assert_eq!(
                query.get("carrier").map(|value| value.as_ref()),
                Some("USPS")
            );
        }
        let length = headers
            .lines()
            .filter_map(|line| line.split_once(':'))
            .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
            .map_or(0, |(_, length)| length.trim().parse::<usize>().unwrap());
        assert!(length <= 16 * 1024);
        let mut body = vec![0; length];
        stream.read_exact(&mut body).unwrap();
        assert!(body.is_empty());
        let tracker = TrackerReply {
            id: "trk_feed_fixture",
            tracking_code: "EVENTS005",
            carrier: "USPS",
            status,
            est_delivery_date: if index == 0 {
                "2031-02-17T13:00:00Z"
            } else {
                "2031-02-18T13:00:00Z"
            },
        };
        let body = if index == 0 {
            // The ordinary worker adopts an existing provider tracker. That
            // first observation emits the same durable transition as a POST.
            #[derive(Serialize)]
            struct TrackerPage {
                trackers: Vec<TrackerReply>,
                has_more: bool,
            }
            serde_json::to_string(&TrackerPage {
                trackers: vec![tracker],
                has_more: false,
            })
            .unwrap()
        } else {
            serde_json::to_string(&tracker).unwrap()
        };
        write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
    }
}

impl Drop for Provider {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let joined = thread.join();
            if !std::thread::panicking() {
                joined.unwrap();
            }
        }
    }
}

#[tokio::test]
async fn real_carrier_worker_preserves_every_transition_and_ignores_identical_observations() {
    let fixture = Fixture::new().await;
    let provider = Provider::start(vec![
        Status::OutForDelivery,
        Status::OutForDelivery,
        Status::OutForDelivery,
        Status::Delivered,
        Status::Failure,
    ]);
    let client = EasyPostClient::for_loopback("example-provider-fixture", &provider.url).unwrap();
    let worker = SyncWorker::new(fixture.store.pool().clone(), std::sync::Arc::new(client));
    let shipment = fixture.shipment("EVENTS005").await;
    let mut shipment = fixture
        .store
        .update(
            shipment.id,
            shipment.revision,
            UpdateShipment {
                direction: Patch {
                    value: Some(Some(Direction::Inbound)),
                },
                ..Default::default()
            },
            "owner",
            None,
        )
        .await
        .unwrap();
    for index in 0..5 {
        if index == 4 {
            shipment = fixture
                .store
                .refresh(shipment.id, shipment.revision, "owner")
                .await
                .unwrap();
        }
        sqlx::query("UPDATE shipments SET next_attempt_at=unixepoch() WHERE id=?")
            .bind(shipment.id.to_string())
            .execute(fixture.store.pool())
            .await
            .unwrap();
        let previous_revision = shipment.revision;
        assert!(worker.tick().await.unwrap());
        shipment = fixture.store.get(shipment.id).await.unwrap();
        if index == 1 {
            // A changed ETA writes a real carrier observation while effective
            // status stays out-for-delivery; that must not notify again.
            assert!(shipment.revision > previous_revision);
            let estimates = fixture.store.delivery_estimates(None, 200).await.unwrap();
            assert_eq!(estimates.len(), 1);
            assert_eq!(estimates[0].estimate.id, shipment.id);
            assert_eq!(
                estimates[0].estimate.estimated_delivery_at,
                "2031-02-18T13:00:00Z"
            );
            assert_eq!(
                fixture
                    .store
                    .state_events(Some(0), 200)
                    .await
                    .unwrap()
                    .items
                    .len(),
                1
            );
        }
        if index == 2 {
            assert_eq!(shipment.revision, previous_revision);
        }
    }
    let events = fixture.store.state_events(Some(0), 200).await.unwrap();
    assert_eq!(
        events
            .items
            .iter()
            .map(|event| event.status)
            .collect::<Vec<_>>(),
        [Status::OutForDelivery, Status::Delivered, Status::Failure]
    );
    assert_eq!(
        events
            .items
            .iter()
            .map(|event| event.event_type)
            .collect::<Vec<_>>(),
        [
            StateEventType::OutForDelivery,
            StateEventType::Delivered,
            StateEventType::Exception
        ]
    );
    assert!(
        events
            .items
            .iter()
            .all(|event| event.shipment_id == shipment.id)
    );
    fixture.store.close().await.unwrap();
}

async fn create_estimate(
    store: &Store,
    number: &str,
    direction: Direction,
    status: Status,
    eta: Option<&str>,
) -> Shipment {
    let carrier = "usps".parse().unwrap();
    let tracker = Tracker {
        provider: shipments::shipping_api::types::ProviderId::EasyPost,
        id: format!("trk_fixture_{number}"),
        tracking_code: number.into(),
        carrier,
        status,
        public_url: None,
        carrier_tracking: CarrierTracking {
            estimated_delivery_at: eta.map(str::to_owned),
            ..Default::default()
        },
    };
    store
        .create(
            CreateShipment {
                title: "Example calendar parcel".into(),
                direction,
                merchant: None,
                order_reference: None,
                signature_required: None,
                tracking: InputTracking {
                    recipient_url: None,
                    carrier: Some(carrier),
                    number: number.into(),
                },
                source: None,
            },
            "owner",
            number,
            Some(&tracker),
        )
        .await
        .unwrap()
}

#[tokio::test]
async fn calendar_projection_filters_and_pages_normalized_instants_without_tracking_history() {
    let fixture = Fixture::new().await;
    let mut expected = Vec::new();
    for (index, (direction, status, archived, eta)) in [
        (
            Direction::Inbound,
            Status::PreTransit,
            false,
            Some("2031-02-18T13:00:00Z"),
        ),
        (
            Direction::Inbound,
            Status::InTransit,
            false,
            Some("2031-02-17T13:00:00-05:00"),
        ),
        (
            Direction::Inbound,
            Status::OutForDelivery,
            false,
            Some("2031-02-17T18:00:00Z"),
        ),
        (
            Direction::Outbound,
            Status::InTransit,
            false,
            Some("2031-02-17T18:00:00Z"),
        ),
        (
            Direction::Inbound,
            Status::Delivered,
            false,
            Some("2031-02-17T18:00:00Z"),
        ),
        (
            Direction::Inbound,
            Status::ReturnToSender,
            false,
            Some("2031-02-17T18:00:00Z"),
        ),
        (
            Direction::Inbound,
            Status::Cancelled,
            false,
            Some("2031-02-17T18:00:00Z"),
        ),
        (
            Direction::Inbound,
            Status::InTransit,
            true,
            Some("2031-02-17T18:00:00Z"),
        ),
        (Direction::Inbound, Status::InTransit, false, None),
        (
            Direction::Inbound,
            Status::InTransit,
            false,
            Some("invalid-date"),
        ),
        (
            Direction::Inbound,
            Status::Failure,
            false,
            Some("2031-02-19T13:00:00Z"),
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let shipment = create_estimate(
            &fixture.store,
            &format!("CAL{index:06}"),
            direction,
            status,
            eta,
        )
        .await;
        if archived {
            fixture
                .store
                .archive(shipment.id, shipment.revision, true, "owner")
                .await
                .unwrap();
        }
        if [0, 1, 2, 10].contains(&index) {
            let instant = time::OffsetDateTime::parse(
                eta.unwrap(),
                &time::format_description::well_known::Rfc3339,
            )
            .unwrap()
            .unix_timestamp();
            expected.push((instant, shipment.id));
        }
    }
    expected.sort_unstable();
    let expected = expected.into_iter().map(|(_, id)| id).collect::<Vec<_>>();
    let all = fixture.store.delivery_estimates(None, 200).await.unwrap();
    assert_eq!(
        all.iter()
            .map(|entry| entry.estimate.id)
            .collect::<Vec<_>>(),
        expected
    );
    assert!(
        all.iter()
            .all(|entry| entry.estimate.direction == Direction::Inbound
                && !entry.estimate.status.is_terminal())
    );
    let server = api::start(
        "127.0.0.1:0".parse().unwrap(),
        Arc::new(
            api::Context::new(fixture.store.clone(), OWNER, None, None)
                .with_integration(INTEGRATION),
        ),
    )
    .unwrap();
    let origin = format!("http://{}", server.local_addr());
    let client = Client::new();
    assert_eq!(
        client
            .get(format!("{origin}/v1/delivery-estimates"))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    let mut ids = Vec::new();
    let mut cursor = None;
    loop {
        let request = client
            .get(format!("{origin}/v1/delivery-estimates"))
            .bearer_auth(INTEGRATION)
            .query(&[("limit", "1")]);
        let request = if let Some(cursor) = &cursor {
            request.query(&[("page_token", cursor)])
        } else {
            request
        };
        let response = request.send().await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["cache-control"], "no-store");
        let page: ResultsPage<DeliveryEstimate> = response.json().await.unwrap();
        assert_eq!(page.items.len(), 1);
        assert_eq!(page.items[0].title, "Example calendar parcel");
        assert_eq!(page.items[0].carrier, Some("usps".parse().unwrap()));
        ids.push(page.items[0].id);
        cursor = page.next_page;
        if cursor.is_none() {
            break;
        }
    }
    assert_eq!(ids, expected);
    server.close().await.unwrap();
    fixture.store.close().await.unwrap();
    if let Ok(path) = std::env::var("SHIPMENTS_TEST_CALENDAR_DATABASE") {
        std::fs::copy(&fixture.path, path).unwrap();
    }
}

#[derive(Serialize)]
struct QueryEvidence {
    feed: Vec<String>,
    tail_checkpoint: Vec<String>,
    calendar_previous_full_snapshot: Vec<String>,
    calendar_first_page: Vec<String>,
    calendar_continuation: Vec<String>,
    previous_snapshot_before: Vec<String>,
    previous_snapshot_after: Vec<String>,
    event_insert_instructions_before: usize,
    event_insert_instructions_after: usize,
    representative_shipments: usize,
    representative_source_events: i64,
    representative_state_events: i64,
}

#[tokio::test]
async fn event_feed_and_atomic_trigger_use_real_indexed_queries() {
    const FEED: &str = "SELECT id,shipment_id,event_type,status,title,carrier,direction,at FROM state_events WHERE id>? ORDER BY id LIMIT ?";
    const PREVIOUS: &str = "SELECT json_extract(previous.snapshot, '$.status') FROM events AS previous WHERE previous.shipment_id=? AND previous.id<? AND previous.snapshot IS NOT NULL ORDER BY previous.id DESC LIMIT 1";
    const INSERT: &str =
        "INSERT INTO events (shipment_id,kind,actor_id,detail,snapshot) VALUES (?,?,?,?,?)";
    let fixture = Fixture::new().await;
    let mut selected = None;
    for index in 0..120 {
        let shipment = fixture.shipment(&format!("PLAN{index:06}")).await;
        let shipment = fixture.correct(&shipment, Status::OutForDelivery).await;
        let shipment = fixture.correct(&shipment, Status::Delivered).await;
        if index == 60 {
            selected = Some(shipment);
        }
    }
    let shipment = selected.unwrap();
    for index in 0..120 {
        create_estimate(
            &fixture.store,
            &format!("CALPLAN{index:06}"),
            Direction::Inbound,
            Status::InTransit,
            Some(if index % 2 == 0 {
                "2031-02-17T18:00:00Z"
            } else {
                "2031-02-18T18:00:00Z"
            }),
        )
        .await;
    }
    let estimates = fixture.store.delivery_estimates(None, 120).await.unwrap();
    assert_eq!(estimates.len(), 120);
    let calendar_after = &estimates[59].position;
    sqlx::query("ANALYZE")
        .execute(fixture.store.pool())
        .await
        .unwrap();
    let mut connection = fixture.store.pool().acquire().await.unwrap();
    let head: i64 = sqlx::query_scalar("SELECT max(id) FROM events")
        .fetch_one(&mut *connection)
        .await
        .unwrap();
    let mut feed_plan = QueryBuilder::<Sqlite>::new("EXPLAIN QUERY PLAN ");
    feed_plan.push(FEED);
    let feed = feed_plan
        .build()
        .bind(head / 2)
        .bind(201_i64)
        .fetch_all(&mut *connection)
        .await
        .unwrap();
    let feed: Vec<String> = feed.iter().map(|row| row.get("detail")).collect();
    assert!(feed.iter().any(|line| line.contains("INTEGER PRIMARY KEY")));
    assert!(!feed.iter().any(|line| line.contains("TEMP B-TREE")));
    let tail = sqlx::query("EXPLAIN QUERY PLAN SELECT coalesce(max(id),0) FROM state_events")
        .fetch_all(&mut *connection)
        .await
        .unwrap();
    let tail_checkpoint: Vec<String> = tail.iter().map(|row| row.get("detail")).collect();
    assert!(
        tail_checkpoint
            .iter()
            .any(|line| line.starts_with("SEARCH"))
    );
    // These are the exact SQL shapes generated by the previous full-shipment
    // poll and the new bounded calendar projection, with representative binds.
    const CALENDAR_BEFORE: &str = r#"EXPLAIN QUERY PLAN SELECT shipments.*,expected_sort_key AS list_sort_key FROM shipments WHERE archived=? AND direction=? AND coalesce(override_status,carrier_status,'"expected"') NOT IN (?,?,?) ORDER BY expected_sort_key,id LIMIT ?"#;
    const CALENDAR: &str = r#"SELECT id,title,tracking_carrier,coalesce(override_status,carrier_status,'"expected"') AS status,json_extract(carrier_tracking,'$.estimated_delivery_at') AS estimated_delivery_at,expected_sort_key FROM shipments WHERE archived=0 AND direction='inbound' AND expected_sort_key<9223372036854775807 AND coalesce(override_status,carrier_status,'"expected"') NOT IN ('"delivered"','"return_to_sender"','"cancelled"')"#;
    let previous = sqlx::query(CALENDAR_BEFORE)
        .bind(false)
        .bind("inbound")
        .bind("\"delivered\"")
        .bind("\"return_to_sender\"")
        .bind("\"cancelled\"")
        .bind(201_i64)
        .fetch_all(&mut *connection)
        .await
        .unwrap();
    let calendar_previous_full_snapshot: Vec<String> =
        previous.iter().map(|row| row.get("detail")).collect();
    let mut first = QueryBuilder::<Sqlite>::new("EXPLAIN QUERY PLAN ");
    first
        .push(CALENDAR)
        .push(" ORDER BY expected_sort_key,id LIMIT ")
        .push_bind(201_i64);
    let first = first.build().fetch_all(&mut *connection).await.unwrap();
    let calendar_first_page: Vec<String> = first.iter().map(|row| row.get("detail")).collect();
    let mut next = QueryBuilder::<Sqlite>::new("EXPLAIN QUERY PLAN ");
    next.push(CALENDAR)
        .push(" AND (expected_sort_key,id)>(")
        .push_bind(calendar_after.sort_key)
        .push(",")
        .push_bind(calendar_after.id.to_string())
        .push(") ORDER BY expected_sort_key,id LIMIT ")
        .push_bind(201_i64);
    let next = next.build().fetch_all(&mut *connection).await.unwrap();
    let calendar_continuation: Vec<String> = next.iter().map(|row| row.get("detail")).collect();
    for details in [
        &calendar_previous_full_snapshot,
        &calendar_first_page,
        &calendar_continuation,
    ] {
        assert!(
            details
                .iter()
                .any(|line| line.contains("shipments_direction_expected_delivery"))
        );
        assert!(!details.iter().any(|line| line.contains("TEMP B-TREE")));
    }
    let trigger: String = sqlx::query_scalar(
        "SELECT sql FROM sqlite_schema WHERE type='trigger' AND name='shipment_state_event'",
    )
    .fetch_one(&mut *connection)
    .await
    .unwrap();
    sqlx::query("DROP TRIGGER shipment_state_event")
        .execute(&mut *connection)
        .await
        .unwrap();
    let mut previous_before = QueryBuilder::<Sqlite>::new("EXPLAIN QUERY PLAN ");
    previous_before.push(PREVIOUS);
    let previous = previous_before
        .build()
        .bind(shipment.id.to_string())
        .bind(head + 1)
        .fetch_all(&mut *connection)
        .await
        .unwrap();
    let previous_snapshot_before: Vec<String> =
        previous.iter().map(|row| row.get("detail")).collect();
    assert!(
        previous_snapshot_before
            .iter()
            .any(|line| line.contains("events_shipment"))
    );
    let snapshot = serde_json::to_string(&shipment).unwrap();
    let mut insert_before = QueryBuilder::<Sqlite>::new("EXPLAIN ");
    insert_before.push(INSERT);
    let before = insert_before
        .build()
        .persistent(false)
        .bind(shipment.id.to_string())
        .bind("\"carrier_updated\"")
        .bind("easypost")
        .bind("Example carrier observation")
        .bind(&snapshot)
        .fetch_all(&mut *connection)
        .await
        .unwrap();
    // Restore the exact trigger installed by the real migration. The SQL is
    // obtained from this disposable database, with no untrusted input.
    let mut restore_trigger = QueryBuilder::<Sqlite>::new("");
    restore_trigger.push(trigger);
    restore_trigger
        .build()
        .execute(&mut *connection)
        .await
        .unwrap();
    let mut insert_after = QueryBuilder::<Sqlite>::new("EXPLAIN ");
    insert_after.push(INSERT);
    let after = insert_after
        .build()
        .persistent(false)
        .bind(shipment.id.to_string())
        .bind("\"carrier_updated\"")
        .bind("easypost")
        .bind("Example carrier observation")
        .bind(&snapshot)
        .fetch_all(&mut *connection)
        .await
        .unwrap();
    assert!(after.len() > before.len());
    let mut previous_after = QueryBuilder::<Sqlite>::new("EXPLAIN QUERY PLAN ");
    previous_after.push(PREVIOUS);
    let previous = previous_after
        .build()
        .bind(shipment.id.to_string())
        .bind(head + 1)
        .fetch_all(&mut *connection)
        .await
        .unwrap();
    let previous_snapshot_after: Vec<String> =
        previous.iter().map(|row| row.get("detail")).collect();
    assert_eq!(previous_snapshot_before, previous_snapshot_after);
    let source_count: i64 = sqlx::query_scalar("SELECT count(*) FROM events")
        .fetch_one(&mut *connection)
        .await
        .unwrap();
    let state_count: i64 = sqlx::query_scalar("SELECT count(*) FROM state_events")
        .fetch_one(&mut *connection)
        .await
        .unwrap();
    assert_eq!(source_count, 480);
    assert_eq!(state_count, 240);
    let evidence = QueryEvidence {
        feed,
        tail_checkpoint,
        calendar_previous_full_snapshot,
        calendar_first_page,
        calendar_continuation,
        previous_snapshot_before,
        previous_snapshot_after,
        event_insert_instructions_before: before.len(),
        event_insert_instructions_after: after.len(),
        representative_shipments: 240,
        representative_source_events: source_count,
        representative_state_events: state_count,
    };
    let evidence = serde_json::to_string_pretty(&evidence).unwrap();
    println!("{evidence}");
    if let Ok(path) = std::env::var("SHIPMENTS_QUERY_PLAN_OUTPUT") {
        std::fs::write(path, evidence).unwrap();
    }
    drop(connection);
    fixture.store.close().await.unwrap();
}

#[tokio::test]
async fn native_adapter_fixture_uses_the_real_store() {
    let Ok(path) = std::env::var("SHIPMENTS_TEST_NATIVE_DATABASE") else {
        return;
    };
    let store = Store::open(std::path::Path::new(&path)).await.unwrap();
    for (index, title, direction, number) in [
        (0, "Office supplies", Direction::Inbound, "TEST123"),
        (1, "Second parcel", Direction::Outbound, "TEST456"),
    ] {
        store
            .create(
                CreateShipment {
                    title: title.into(),
                    direction,
                    merchant: None,
                    order_reference: None,
                    signature_required: None,
                    tracking: InputTracking {
                        recipient_url: None,
                        carrier: Some(shipments::carrier::Carrier::Usps),
                        number: number.into(),
                    },
                    source: None,
                },
                "fixture",
                &format!("native-adapter-{index}"),
                None,
            )
            .await
            .unwrap();
    }
}
