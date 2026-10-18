use std::path::PathBuf;

use shipments::{
    carrier::Carrier,
    error::Error,
    models::{CarrierEvent, CarrierTracking, Direction, Status},
    store::Store,
};
use sqlx::{
    SqlitePool,
    migrate::{MigrateError, Migrator},
    sqlite::{SqliteConnectOptions, SqlitePoolOptions},
};
use uuid::Uuid;

const ACTIVE_NUMBER: &str = "9400100000000000000001";
const ARCHIVED_NUMBER: &str = "9400100000000000000004";

struct Fixture {
    _directory: tempfile::TempDir,
    path: PathBuf,
    pool: SqlitePool,
}

impl Fixture {
    async fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let migrations = directory.path().join("before-global-tracking-number");
        std::fs::create_dir(&migrations).unwrap();
        for entry in
            std::fs::read_dir(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations"))
                .unwrap()
        {
            let entry = entry.unwrap();
            let name = entry.file_name();
            if name
                .to_str()
                .unwrap()
                .split_once('_')
                .unwrap()
                .0
                .parse::<i64>()
                .unwrap()
                <= 16
            {
                std::fs::copy(entry.path(), migrations.join(name)).unwrap();
            }
        }
        let path = directory.path().join("global-tracking-number.sqlite3");
        let pool = raw_pool(&path).await;
        Migrator::new(migrations.as_path())
            .await
            .unwrap()
            .run(&pool)
            .await
            .unwrap();
        Self {
            _directory: directory,
            path,
            pool,
        }
    }

    async fn seed(
        &self,
        number: &str,
        carrier: Carrier,
        direction: Direction,
        archived: bool,
    ) -> Uuid {
        let id = insert(&self.pool, number, carrier, direction, archived)
            .await
            .unwrap();
        sqlx::query("INSERT INTO events(shipment_id,kind,actor_id,detail) VALUES(?,'\"created\"','migration-test','Original tracking record')")
            .bind(id.to_string()).execute(&self.pool).await.unwrap();
        sqlx::query("INSERT INTO sources(namespace,key,shipment_id) VALUES('migration',?,?)")
            .bind(id.to_string())
            .bind(id.to_string())
            .execute(&self.pool)
            .await
            .unwrap();
        id
    }
}

async fn raw_pool(path: &std::path::Path) -> SqlitePool {
    SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(
            SqliteConnectOptions::new()
                .filename(path)
                .create_if_missing(true)
                .foreign_keys(true),
        )
        .await
        .unwrap()
}

async fn insert(
    pool: &SqlitePool,
    number: &str,
    carrier: Carrier,
    direction: Direction,
    archived: bool,
) -> Result<Uuid, sqlx::Error> {
    let id = Uuid::now_v7();
    let history = CarrierTracking {
        events: vec![CarrierEvent {
            status: Some(Status::InTransit),
            message: Some("Original carrier scan".into()),
            datetime: Some("2026-01-01T00:00:00Z".into()),
            ..Default::default()
        }],
        ..Default::default()
    };
    sqlx::query("INSERT INTO shipments(id,title,tracking_number,tracking_carrier,direction,archived,carrier_status,carrier_tracking,tracker_id,registration_started_at,registration_state,next_attempt_at) VALUES(?,'Global tracking migration',?,?,?,?,?,?,?,1700000000,'registered',unixepoch()+3600)")
        .bind(id.to_string()).bind(number).bind(carrier.as_db_key())
        .bind(direction.as_str()).bind(archived).bind(serde_json::to_string(&Status::InTransit).unwrap())
        .bind(serde_json::to_string(&history).unwrap()).bind(format!("trk_{}", id.simple()))
        .execute(pool).await?;
    Ok(id)
}

#[derive(Debug, PartialEq, Eq, sqlx::FromRow)]
struct ShipmentRow {
    id: String,
    tracking_number: String,
    tracking_carrier: String,
    direction: String,
    archived: bool,
    carrier_status: String,
    carrier_tracking: String,
    tracker_id: String,
    registration_started_at: i64,
    registration_state: String,
    next_attempt_at: i64,
}

#[derive(Debug, PartialEq, Eq, sqlx::FromRow)]
struct EventRow {
    id: i64,
    shipment_id: String,
    kind: String,
    actor_id: String,
    at: String,
    detail: String,
    snapshot: Option<String>,
}

#[derive(Debug, PartialEq, Eq, sqlx::FromRow)]
struct SourceRow {
    namespace: String,
    key: String,
    shipment_id: String,
}

#[derive(Debug, PartialEq, Eq)]
struct Snapshot {
    shipments: Vec<ShipmentRow>,
    events: Vec<EventRow>,
    sources: Vec<SourceRow>,
}

impl Snapshot {
    async fn load(pool: &SqlitePool) -> Self {
        Self {
            shipments: sqlx::query_as("SELECT id,tracking_number,tracking_carrier,direction,archived,carrier_status,carrier_tracking,tracker_id,registration_started_at,registration_state,next_attempt_at FROM shipments ORDER BY id")
                .fetch_all(pool).await.unwrap(),
            events: sqlx::query_as("SELECT * FROM events ORDER BY id")
                .fetch_all(pool).await.unwrap(),
            sources: sqlx::query_as("SELECT * FROM sources ORDER BY namespace,key")
                .fetch_all(pool).await.unwrap(),
        }
    }
}

#[derive(Debug, PartialEq, Eq, sqlx::FromRow)]
struct TrackingIndex {
    name: String,
    is_unique: bool,
    partial: bool,
}

async fn tracking_indexes(pool: &SqlitePool) -> Vec<TrackingIndex> {
    sqlx::query_as("SELECT name,\"unique\" AS is_unique,partial FROM pragma_index_list('shipments') WHERE name IN ('shipments_active_tracking','shipments_tracking_number') ORDER BY name")
        .fetch_all(pool).await.unwrap()
}

fn assert_unique_violation(error: sqlx::Error) {
    assert!(
        error
            .as_database_error()
            .is_some_and(|database| database.is_unique_violation()),
        "expected a uniqueness violation, got {error}"
    );
}

#[tokio::test]
async fn upgrade_preserves_shipments_and_globally_enforces_tracking_numbers() {
    let fixture = Fixture::new().await;
    let usps: Carrier = "USPS".parse().unwrap();
    let ups: Carrier = "UPS".parse().unwrap();
    let first = fixture
        .seed(ACTIVE_NUMBER, usps, Direction::Inbound, false)
        .await;
    let second = fixture
        .seed("9400100000000000000002", ups, Direction::Outbound, false)
        .await;
    fixture
        .seed("9400100000000000000003", usps, Direction::Inbound, true)
        .await;
    fixture
        .seed(ARCHIVED_NUMBER, ups, Direction::Outbound, true)
        .await;
    let before = Snapshot::load(&fixture.pool).await;
    fixture.pool.close().await;
    let store = Store::open(&fixture.path).await.unwrap();
    assert_eq!(Snapshot::load(store.pool()).await, before);
    for number in [ACTIVE_NUMBER, ARCHIVED_NUMBER] {
        for carrier in [usps, ups] {
            for archived in [false, true] {
                assert_unique_violation(
                    insert(store.pool(), number, carrier, Direction::Outbound, archived)
                        .await
                        .unwrap_err(),
                );
            }
        }
        assert_unique_violation(
            sqlx::query("UPDATE shipments SET tracking_number=?,tracking_carrier='ups',archived=1 WHERE id=?")
                .bind(number).bind(second.to_string()).execute(store.pool()).await.unwrap_err(),
        );
    }
    assert_eq!(Snapshot::load(store.pool()).await, before);
    assert_eq!(
        tracking_indexes(store.pool()).await,
        vec![TrackingIndex {
            name: "shipments_tracking_number".into(),
            is_unique: true,
            partial: false,
        }]
    );
    let columns: Vec<String> = sqlx::query_scalar(
        "SELECT name FROM pragma_index_info('shipments_tracking_number') ORDER BY seqno",
    )
    .fetch_all(store.pool())
    .await
    .unwrap();
    assert_eq!(columns, vec!["tracking_number"]);
    sqlx::query("UPDATE shipments SET tracking_number=tracking_number,tracking_carrier='ups',direction='outbound',carrier_status=?,archived=1 WHERE id=?")
        .bind(serde_json::to_string(&Status::Delivered).unwrap()).bind(first.to_string())
        .execute(store.pool()).await.unwrap();
    let changed = store.get(first).await.unwrap();
    assert_eq!(changed.tracking.number, ACTIVE_NUMBER);
    assert_eq!(changed.tracking.carrier, ups);
    assert_eq!(changed.status, Status::Delivered);
    assert_eq!(changed.direction, Direction::Outbound);
    assert!(changed.archived);
    assert_eq!(changed.carrier_tracking.events.len(), 1);
    let after = Snapshot::load(store.pool()).await;
    assert_eq!(after.events, before.events);
    assert_eq!(after.sources, before.sources);
    store.close().await.unwrap();
    let reopened = Store::open(&fixture.path).await.unwrap();
    assert_eq!(Snapshot::load(reopened.pool()).await, after);
    reopened.close().await.unwrap();
}

#[tokio::test]
async fn legacy_duplicates_fail_upgrade_atomically_without_deleting_history_or_the_old_index() {
    let fixture = Fixture::new().await;
    let usps = "USPS".parse().unwrap();
    fixture
        .seed(ACTIVE_NUMBER, usps, Direction::Inbound, false)
        .await;
    fixture
        .seed(
            ACTIVE_NUMBER,
            "UPS".parse().unwrap(),
            Direction::Outbound,
            false,
        )
        .await;
    fixture
        .seed(ACTIVE_NUMBER, usps, Direction::Inbound, true)
        .await;
    fixture
        .seed(ACTIVE_NUMBER, usps, Direction::Outbound, true)
        .await;
    let before = Snapshot::load(&fixture.pool).await;
    let old_indexes = tracking_indexes(&fixture.pool).await;
    assert_eq!(
        old_indexes,
        vec![TrackingIndex {
            name: "shipments_active_tracking".into(),
            is_unique: true,
            partial: true,
        }]
    );
    fixture.pool.close().await;
    for _ in 0..2 {
        match Store::open(&fixture.path).await {
            Err(Error::Migration(MigrateError::ExecuteMigration(error, 17))) => {
                assert_unique_violation(error);
            }
            Err(error) => panic!("unexpected migration failure: {error}"),
            Ok(store) => {
                store.close().await.unwrap();
                panic!("legacy duplicates must fail migration 0017");
            }
        }
        let pool = raw_pool(&fixture.path).await;
        assert_eq!(Snapshot::load(&pool).await, before);
        assert_eq!(tracking_indexes(&pool).await, old_indexes);
        let version: i64 = sqlx::query_scalar("SELECT max(version) FROM _sqlx_migrations")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(version, 16);
        assert_unique_violation(
            insert(&pool, ACTIVE_NUMBER, usps, Direction::Inbound, false)
                .await
                .unwrap_err(),
        );
        pool.close().await;
    }
}
