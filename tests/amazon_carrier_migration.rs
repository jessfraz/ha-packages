//! Correct historical account-owned labels without rewriting shipment history.

use std::path::Path;

use shipments::{
    carrier::Carrier,
    models::{
        CarrierEvent, CarrierTracking, Direction, RegistrationState, Shipment, Source, Status,
        StatusOverride, SyncState, Tracking,
    },
    shipping_api::types::ProviderId,
    store::Store,
};
use sqlx::{
    FromRow, SqlitePool,
    migrate::Migrator,
    sqlite::{SqliteConnectOptions, SqlitePoolOptions},
};
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, Eq, FromRow)]
struct ShipmentRow {
    id: String,
    revision: i64,
    title: String,
    merchant: Option<String>,
    order_reference: Option<String>,
    tracking_carrier: String,
    tracking_number: String,
    tracking_generation: i64,
    carrier_status: Option<String>,
    override_status: Option<String>,
    override_reason: Option<String>,
    override_actor: Option<String>,
    override_at: Option<String>,
    archived: bool,
    created_at: String,
    updated_at: String,
    tracker_id: Option<String>,
    last_success_at: Option<String>,
    last_error: Option<String>,
    next_attempt_at: Option<i64>,
    attempts: i64,
    lease_until: Option<i64>,
    lease_token: Option<String>,
    tracking_url: Option<String>,
    last_attempt_at: Option<i64>,
    status_since: Option<String>,
    signature_required: Option<bool>,
    carrier_tracking: String,
    expected_sort_key: i64,
    delivered_sort_key: i64,
    direction: String,
    registration_state: String,
    registration_started_at: Option<i64>,
    last_error_code: Option<String>,
    last_provider_status: Option<i64>,
    last_provider_error_code: Option<String>,
    tracking_provider: String,
    recipient_url: Option<String>,
    calendar_sort_key: i64,
}

#[derive(Clone, Debug, PartialEq, Eq, FromRow)]
struct HistoryRow {
    id: i64,
    shipment_id: String,
    kind: String,
    actor_id: String,
    at: String,
    detail: String,
    snapshot: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, FromRow)]
struct StateEventRow {
    id: i64,
    shipment_id: String,
    event_type: String,
    status: String,
    title: String,
    carrier: Option<String>,
    direction: String,
    at: String,
}

async fn old_database(directory: &Path) -> (std::path::PathBuf, SqlitePool) {
    let migrations = directory.join("before-account-carrier");
    std::fs::create_dir(&migrations).unwrap();
    for entry in
        std::fs::read_dir(Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations")).unwrap()
    {
        let entry = entry.unwrap();
        let name = entry.file_name();
        let version = name
            .to_str()
            .unwrap()
            .split_once('_')
            .unwrap()
            .0
            .parse::<i64>()
            .unwrap();
        if version <= 21 {
            std::fs::copy(entry.path(), migrations.join(name)).unwrap();
        }
    }
    let database = directory.join("legacy.sqlite3");
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(
            SqliteConnectOptions::new()
                .filename(&database)
                .create_if_missing(true)
                .foreign_keys(true),
        )
        .await
        .unwrap();
    Migrator::new(migrations.as_path())
        .await
        .unwrap()
        .run(&pool)
        .await
        .unwrap();
    (database, pool)
}

fn historical_shipment(
    id: Uuid,
    carrier: Carrier,
    provider: ProviderId,
    number: String,
    archived: bool,
) -> Shipment {
    Shipment {
        id,
        revision: 3,
        title: "Original title".into(),
        direction: Direction::Inbound,
        merchant: Some("Original merchant".into()),
        order_reference: Some("111-0000000-0000001".into()),
        signature_required: Some(true),
        tracking: Tracking {
            carrier,
            number,
            recipient_url: None,
        },
        status: Status::Delivered,
        carrier_status: Some(Status::InTransit),
        carrier_tracking: CarrierTracking {
            events: vec![CarrierEvent {
                status: Some(Status::InTransit),
                status_detail: None,
                datetime: Some("2026-10-01T10:00:00Z".into()),
                message: Some("Original scan".into()),
                description: None,
                source: None,
                carrier_code: None,
                tracking_location: None,
            }],
            signed_by: Some("Synthetic recipient".into()),
            estimated_delivery_at: Some("2026-10-02T10:00:00Z".into()),
            delivered_at: Some("2026-10-02T09:00:00Z".into()),
            ..Default::default()
        },
        status_override: Some(StatusOverride {
            status: Status::Delivered,
            reason: "Owner confirmed delivery".into(),
            actor_id: "owner".into(),
            set_at: "2026-10-02T11:00:00Z".into(),
        }),
        archived,
        created_at: "2026-10-01T09:00:00Z".into(),
        updated_at: "2026-10-02T11:00:00Z".into(),
        sources: vec![Source {
            namespace: "fixture-mail".into(),
            key: id.to_string(),
        }],
        sync: SyncState {
            provider,
            registration_state: RegistrationState::Registered,
            registration_started_at: Some(123),
            error_code: Some("provider_http".into()),
            provider_status: Some(503),
            provider_error_code: Some("synthetic-provider-error".into()),
            tracker_id: Some(format!("synthetic-account:{id}")),
            tracking_url: Some("https://www.amazon.com/progress-tracker/package".into()),
            last_success_at: Some("2026-10-02T10:00:00Z".into()),
            last_error: Some("Original provider failure".into()),
            next_attempt_at: Some(5_000_000_000),
            attempts: 4,
        },
    }
}

#[tokio::test]
async fn account_carrier_migration_preserves_records_history_and_unrelated_ownership() {
    let directory = tempfile::tempdir().unwrap();
    let (database, old) = old_database(directory.path()).await;
    let cases = [
        (Carrier::AmazonMcf, ProviderId::AmazonAccount, false, true),
        (Carrier::AmazonMcf, ProviderId::AmazonAccount, true, true),
        (Carrier::AmazonMcf, ProviderId::AfterShip, false, false),
        (Carrier::AmazonMcf, ProviderId::EasyPost, false, false),
        (Carrier::Usps, ProviderId::AmazonAccount, false, false),
        (Carrier::Usps, ProviderId::EasyPost, false, false),
        (
            Carrier::AmazonShipping,
            ProviderId::AmazonAccount,
            false,
            false,
        ),
    ];
    let mut originals = Vec::new();
    for (index, (carrier, provider, archived, corrected)) in cases.into_iter().enumerate() {
        let id = Uuid::now_v7();
        let snapshot =
            historical_shipment(id, carrier, provider, format!("TBA{index:012}"), archived);
        sqlx::query("INSERT INTO shipments(id,revision,title,merchant,order_reference,tracking_carrier,tracking_number,tracking_generation,carrier_status,override_status,override_reason,override_actor,override_at,archived,created_at,updated_at,tracker_id,last_success_at,last_error,next_attempt_at,attempts,lease_until,lease_token,tracking_url,last_attempt_at,status_since,signature_required,carrier_tracking,direction,registration_state,registration_started_at,last_error_code,last_provider_status,last_provider_error_code,tracking_provider) VALUES(?,7,'Original title','Original merchant','111-0000000-0000001',?,?,9,'\"in_transit\"','\"delivered\"','Owner confirmed delivery','owner','2026-10-02T11:00:00Z',?,'2026-10-01T09:00:00Z','2026-10-02T11:00:00Z',?,'2026-10-02T10:00:00Z','Original provider failure',5000000000,4,5000000010,'old-worker',?,1234,'2026-10-01T10:00:00Z',1,?,'inbound','registered',123,'provider_http',503,'synthetic-provider-error',?)")
            .bind(id.to_string()).bind(carrier.as_db_key()).bind(&snapshot.tracking.number)
            .bind(archived).bind(&snapshot.sync.tracker_id).bind(&snapshot.sync.tracking_url)
            .bind(serde_json::to_string(&snapshot.carrier_tracking).unwrap()).bind(provider.as_str())
            .execute(&old).await.unwrap();
        sqlx::query("INSERT INTO sources(namespace,key,shipment_id) VALUES('fixture-mail',?,?)")
            .bind(id.to_string())
            .bind(id.to_string())
            .execute(&old)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO amazon_parcels(source_key,shipment_id,tracking_number) VALUES(?,?,?)",
        )
        .bind(format!("synthetic-account:{id}"))
        .bind(id.to_string())
        .bind(&snapshot.tracking.number)
        .execute(&old)
        .await
        .unwrap();
        sqlx::query("INSERT INTO events(shipment_id,kind,actor_id,detail,snapshot) VALUES(?,'\"created\"','owner','Original event',?)")
            .bind(id.to_string()).bind(serde_json::to_string(&snapshot).unwrap()).execute(&old).await.unwrap();
        originals.push((id, corrected));
    }
    let before: Vec<ShipmentRow> = sqlx::query_as("SELECT * FROM shipments ORDER BY id")
        .fetch_all(&old)
        .await
        .unwrap();
    let history: Vec<HistoryRow> = sqlx::query_as("SELECT * FROM events ORDER BY id")
        .fetch_all(&old)
        .await
        .unwrap();
    let state_events: Vec<StateEventRow> = sqlx::query_as("SELECT * FROM state_events ORDER BY id")
        .fetch_all(&old)
        .await
        .unwrap();
    let sources: Vec<(String, String, String)> =
        sqlx::query_as("SELECT namespace,key,shipment_id FROM sources ORDER BY namespace,key")
            .fetch_all(&old)
            .await
            .unwrap();
    let parcels: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT source_key,shipment_id,tracking_number FROM amazon_parcels ORDER BY source_key",
    )
    .fetch_all(&old)
    .await
    .unwrap();
    old.close().await;

    let mut first_open = None;
    for _ in 0..2 {
        let store = Store::open(&database).await.unwrap();
        let after: Vec<ShipmentRow> = sqlx::query_as("SELECT * FROM shipments ORDER BY id")
            .fetch_all(store.pool())
            .await
            .unwrap();
        assert_eq!(after.len(), before.len());
        for (original, current) in before.iter().zip(&after) {
            let corrected = originals
                .iter()
                .any(|(id, corrected)| *corrected && id.to_string() == original.id);
            let id = Uuid::parse_str(&current.id).unwrap();
            let shipment = store.get(id).await.unwrap();
            let mut expected = original.clone();
            if corrected {
                assert_eq!(
                    serde_json::to_string(&shipment.tracking.carrier).unwrap(),
                    "\"AmazonAccount\""
                );
                expected.tracking_carrier = "amazonaccount".into();
                expected.revision += 1;
                expected.tracking_generation += 1;
                expected.lease_token = None;
                expected.lease_until = None;
                assert_ne!(current.updated_at, original.updated_at);
                expected.updated_at.clone_from(&current.updated_at);
            }
            assert_eq!(current, &expected);
            let events = store.events(id, 0, 100).await.unwrap();
            assert_eq!(events.len(), if corrected { 2 } else { 1 });
            assert!(events[0].snapshot.is_some());
        }
        let after_history: Vec<HistoryRow> = sqlx::query_as("SELECT * FROM events ORDER BY id")
            .fetch_all(store.pool())
            .await
            .unwrap();
        assert_eq!(&after_history[..history.len()], history.as_slice());
        assert_eq!(after_history.len(), history.len() + 2);
        for event in &after_history[history.len()..] {
            assert_eq!(event.kind, "\"updated\"");
            assert_eq!(event.actor_id, "system");
            assert_eq!(
                event.detail,
                "Carrier corrected to Amazon Account; tracking ownership preserved"
            );
            assert!(event.snapshot.is_none());
        }
        assert_eq!(
            sqlx::query_as::<_, StateEventRow>("SELECT * FROM state_events ORDER BY id")
                .fetch_all(store.pool())
                .await
                .unwrap(),
            state_events
        );
        assert_eq!(
            sqlx::query_as::<_, (String, String, String)>(
                "SELECT namespace,key,shipment_id FROM sources ORDER BY namespace,key"
            )
            .fetch_all(store.pool())
            .await
            .unwrap(),
            sources
        );
        assert_eq!(sqlx::query_as::<_, (String, String, String)>("SELECT source_key,shipment_id,tracking_number FROM amazon_parcels ORDER BY source_key").fetch_all(store.pool()).await.unwrap(), parcels);
        if let Some((first_rows, first_history)) = &first_open {
            assert_eq!(&after, first_rows);
            assert_eq!(&after_history, first_history);
        } else {
            first_open = Some((after, after_history));
        }
        store.close().await.unwrap();
    }
}
