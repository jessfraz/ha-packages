use std::{
    io::Write,
    net::TcpListener,
    sync::{Arc, Mutex},
};

use reqwest::{Client, StatusCode};
use shipments::{
    api,
    models::{
        CreateShipment, Direction, Event, Patch, RegistrationState, Shipment, Source, Tracking,
        UpdateShipment,
    },
    shipping_api::easypost::EasyPostClient,
    store::Store,
};

const OWNER: &str = "pending-api-owner-credential-at-least-32-bytes";

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

#[derive(serde::Deserialize)]
struct SavedLog {
    event: String,
    request_id: Option<uuid::Uuid>,
    shipment_id: Option<uuid::Uuid>,
    operation: Option<String>,
    outcome: Option<String>,
}

fn input(number: &str) -> CreateShipment {
    CreateShipment {
        title: "Durable pending parcel".into(),
        direction: Direction::Inbound,
        merchant: None,
        order_reference: None,
        signature_required: None,
        tracking: Tracking {
            carrier: Some("usps".parse().unwrap()),
            number: number.into(),
        },
        source: Some(Source {
            namespace: "test-mail".into(),
            key: "pending-message".into(),
        }),
    }
}

#[derive(serde::Deserialize)]
struct Failure {
    error_code: Option<String>,
    message: String,
}

#[tokio::test]
async fn creates_commit_pending_records_sources_history_and_replays_without_provider_io() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("pending.sqlite3");
    let store = Store::open(&path).await.unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let provider = EasyPostClient::for_loopback(
        "unused-pending-test-key",
        &format!("http://{}/", listener.local_addr().unwrap()),
    )
    .unwrap();
    let capture = Capture::default();
    let server = api::start_with_logger(
        "127.0.0.1:0".parse().unwrap(),
        Arc::new(api::Context::new(
            store.clone(),
            OWNER,
            None,
            Some(std::sync::Arc::new(provider)),
        )),
        shipments::logging::with_writer(capture.clone()),
    )
    .unwrap();
    let base = format!("http://{}", server.local_addr());
    let client = Client::builder()
        .timeout(std::time::Duration::from_secs(2))
        .build()
        .unwrap();
    let request = input("9400111899223856926381");
    let mut saved = None;
    for _ in 0..2 {
        let response = client
            .post(format!("{base}/v1/shipments"))
            .bearer_auth(OWNER)
            .header("Idempotency-Key", "durable-pending")
            .json(&request)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
        let request_id: uuid::Uuid = response.headers()["x-request-id"]
            .to_str()
            .unwrap()
            .parse()
            .unwrap();
        let shipment: Shipment = response.json().await.unwrap();
        let output = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
        let logs: Vec<SavedLog> = output
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let saved_log = logs
            .iter()
            .find(|entry| {
                entry.event == "tracking_registration" && entry.request_id == Some(request_id)
            })
            .unwrap();
        assert_eq!(saved_log.shipment_id, Some(shipment.id));
        assert_eq!(saved_log.operation.as_deref(), Some("create"));
        assert_eq!(saved_log.outcome.as_deref(), Some("saved"));
        assert!(!output.contains(&request.tracking.number));
        assert!(!output.contains(&request.title));
        assert!(!output.contains(OWNER));
        assert_eq!(shipment.sync.registration_state, RegistrationState::Pending);
        assert!(shipment.sync.tracker_id.is_none());
        assert!(shipment.sync.registration_started_at.is_none());
        assert!(shipment.sync.next_attempt_at.is_some());
        assert_eq!(shipment.sources, vec![request.source.clone().unwrap()]);
        if let Some(id) = saved {
            assert_eq!(id, shipment.id);
        } else {
            saved = Some(shipment.id);
        }
    }
    assert!(
        matches!(listener.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock)
    );
    let id = saved.unwrap();
    let events = store.events(id, 0, 100).await.unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(
        events[0].snapshot.as_ref().unwrap().sync.registration_state,
        RegistrationState::Pending
    );
    let duplicate = client
        .post(format!("{base}/v1/shipments"))
        .bearer_auth(OWNER)
        .header("Idempotency-Key", "different-create-key")
        .json(&request)
        .send()
        .await
        .unwrap();
    assert_eq!(duplicate.status(), StatusCode::CONFLICT);
    server.close().await.unwrap();
    store.close().await.unwrap();
    let restored = Store::open(&path).await.unwrap();
    let shipment = restored.get(id).await.unwrap();
    assert_eq!(shipment.sync.registration_state, RegistrationState::Pending);
    assert_eq!(shipment.sources, vec![request.source.unwrap()]);
    assert_eq!(restored.events(id, 0, 100).await.unwrap().len(), 1);
    let without_provider = api::start(
        "127.0.0.1:0".parse().unwrap(),
        Arc::new(api::Context::new(restored.clone(), OWNER, None, None)),
    )
    .unwrap();
    // A prefix alone cannot establish provider support: Amazon Shipping is
    // in the canonical catalog and eligibility depends on the account.
    let mut another = input("TBA123456789000");
    another.tracking.carrier = Some("AmazonShipping".parse().unwrap());
    another.source = None;
    let response = client
        .post(format!(
            "http://{}/v1/shipments",
            without_provider.local_addr()
        ))
        .bearer_auth(OWNER)
        .header("Idempotency-Key", "missing-provider")
        .json(&another)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let shipment: Shipment = response.json().await.unwrap();
    assert_eq!(shipment.sync.registration_state, RegistrationState::Pending);
    without_provider.close().await.unwrap();
    restored.close().await.unwrap();
}

#[tokio::test]
async fn local_validation_rejects_garbage_and_tracking_edits_reset_durable_rejections() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(&directory.path().join("pending.sqlite3"))
        .await
        .unwrap();
    let server = api::start(
        "127.0.0.1:0".parse().unwrap(),
        Arc::new(api::Context::new(store.clone(), OWNER, None, None)),
    )
    .unwrap();
    let base = format!("http://{}", server.local_addr());
    let client = Client::new();
    for (number, code) in [
        ("garbage", "tracking_invalid"),
        ("123", "tracking_invalid"),
        ("😈123456", "tracking_invalid"),
    ] {
        let response = client
            .post(format!("{base}/v1/shipments"))
            .bearer_auth(OWNER)
            .header("Idempotency-Key", "bad-local-input")
            .json(&input(number))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let error: Failure = response.json().await.unwrap();
        assert_eq!(error.error_code.as_deref(), Some(code));
        assert!(!error.message.contains(number));
    }
    let created: Shipment = client
        .post(format!("{base}/v1/shipments"))
        .bearer_auth(OWNER)
        .header("Idempotency-Key", "editable-pending")
        .json(&input("9400111899223856926383"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    sqlx::query("UPDATE shipments SET registration_state='rejected',registration_started_at=123,last_error='Carrier rejected the number',last_error_code='tracking_unsupported',last_provider_status=422,last_provider_error_code='TRACKER.NO_CARRIER_FOR_CODE',attempts=3,next_attempt_at=NULL WHERE id=?")
        .bind(created.id.to_string()).execute(store.pool()).await.unwrap();
    let rejected = store.get(created.id).await.unwrap();
    assert_eq!(
        rejected.sync.error_code.as_deref(),
        Some("tracking_unsupported")
    );
    assert_eq!(rejected.sync.provider_status, Some(422));
    assert_eq!(
        rejected.sync.provider_error_code.as_deref(),
        Some("TRACKER.NO_CARRIER_FOR_CODE")
    );
    let refresh = client
        .post(format!("{base}/v1/shipments/{}/refresh", created.id))
        .bearer_auth(OWNER)
        .header("If-Match", "\"1\"")
        .send()
        .await
        .unwrap();
    assert_eq!(refresh.status(), StatusCode::CONFLICT);
    let changed: Shipment = client
        .patch(format!("{base}/v1/shipments/{}", created.id))
        .bearer_auth(OWNER)
        .header("If-Match", "\"1\"")
        .json(&UpdateShipment {
            tracking: Patch {
                value: Some(Some(Tracking {
                    carrier: Some("usps".parse().unwrap()),
                    number: "9400111899223856926384".into(),
                })),
            },
            ..Default::default()
        })
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(changed.revision, 2);
    assert_eq!(changed.sync.registration_state, RegistrationState::Pending);
    assert!(changed.sync.registration_started_at.is_none());
    assert!(changed.sync.error_code.is_none());
    assert!(changed.sync.provider_status.is_none());
    assert!(changed.sync.provider_error_code.is_none());
    assert!(changed.sync.last_error.is_none());
    assert_eq!(changed.sync.attempts, 0);
    assert!(changed.sync.next_attempt_at.is_some());
    let events: Vec<Event> = store.events(changed.id, 0, 100).await.unwrap();
    assert_eq!(events.len(), 2);
    assert_eq!(
        events[1]
            .snapshot
            .as_ref()
            .unwrap()
            .tracking
            .as_ref()
            .unwrap()
            .number,
        changed.tracking.number
    );
    server.close().await.unwrap();
    store.close().await.unwrap();
}

#[tokio::test]
async fn historical_snapshots_and_create_replays_infer_registration_without_changing_explicit_state()
 {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(&directory.path().join("historical.sqlite3"))
        .await
        .unwrap();
    let request = input("9400111899223856926385");
    let created = store
        .create(request.clone(), "owner", "historical-create", None)
        .await
        .unwrap();
    let old_snapshot = serde_json::to_string(&created)
        .unwrap()
        .replace("\"direction\":\"inbound\",", "")
        .replace("\"registration_state\":\"pending\",", "")
        .replace("\"registration_started_at\":null,", "")
        .replace("\"error_code\":null,", "")
        .replace("\"provider_status\":null,", "")
        .replace("\"provider_error_code\":null,", "")
        .replace("\"tracker_id\":null", "\"tracker_id\":\"trk_historical\"");
    sqlx::query("UPDATE shipments SET tracker_id='trk_historical',registration_state='registered' WHERE id=?")
        .bind(created.id.to_string()).execute(store.pool()).await.unwrap();
    sqlx::query("UPDATE events SET snapshot=? WHERE shipment_id=?")
        .bind(&old_snapshot)
        .bind(created.id.to_string())
        .execute(store.pool())
        .await
        .unwrap();
    sqlx::query(
        "UPDATE idempotency SET response=? WHERE actor_id='owner' AND key='historical-create'",
    )
    .bind(&old_snapshot)
    .execute(store.pool())
    .await
    .unwrap();
    assert_eq!(
        store.get(created.id).await.unwrap().sync.registration_state,
        RegistrationState::Registered
    );
    let event = store
        .events(created.id, 0, 10)
        .await
        .unwrap()
        .pop()
        .unwrap();
    assert_eq!(
        event.snapshot.unwrap().sync.registration_state,
        RegistrationState::Registered
    );
    let replay = store
        .replay_create(&request, "owner", "historical-create")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        replay.sync.registration_state,
        RegistrationState::Registered
    );
    assert!(replay.sync.provider_error_code.is_none());
    let explicit: Shipment = serde_json::from_str(&old_snapshot.replace(
        "\"tracker_id\"",
        "\"registration_state\":\"rejected\",\"tracker_id\"",
    ))
    .unwrap();
    assert_eq!(
        explicit.sync.registration_state,
        RegistrationState::Rejected
    );
    let unregistered: Shipment = serde_json::from_str(
        &old_snapshot.replace("\"tracker_id\":\"trk_historical\"", "\"tracker_id\":null"),
    )
    .unwrap();
    assert_eq!(
        unregistered.sync.registration_state,
        RegistrationState::Pending
    );
    store.close().await.unwrap();
}

#[tokio::test]
async fn rejected_records_remain_unscheduled_after_restore_and_manual_corrections() {
    use shipments::models::{SetOverride, Status};
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(&directory.path().join("rejected.sqlite3"))
        .await
        .unwrap();
    let created = store
        .create(
            input("9400111899223856926386"),
            "owner",
            "rejected-lifecycle",
            None,
        )
        .await
        .unwrap();
    sqlx::query(
        "UPDATE shipments SET registration_state='rejected',next_attempt_at=NULL WHERE id=?",
    )
    .bind(created.id.to_string())
    .execute(store.pool())
    .await
    .unwrap();
    let corrected = store
        .set_override(
            created.id,
            created.revision,
            Some(SetOverride {
                status: Status::InTransit,
                reason: "Status correction fixture".into(),
            }),
            "owner",
        )
        .await
        .unwrap();
    assert!(corrected.sync.next_attempt_at.is_none());
    let cleared = store
        .set_override(corrected.id, corrected.revision, None, "owner")
        .await
        .unwrap();
    assert!(cleared.sync.next_attempt_at.is_none());
    let archived = store
        .archive(cleared.id, cleared.revision, true, "owner")
        .await
        .unwrap();
    let restored = store
        .archive(archived.id, archived.revision, false, "owner")
        .await
        .unwrap();
    assert_eq!(
        restored.sync.registration_state,
        RegistrationState::Rejected
    );
    assert!(restored.sync.next_attempt_at.is_none());
    store.close().await.unwrap();
}
