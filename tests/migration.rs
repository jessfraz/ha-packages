use sha2::{Digest, Sha256};
use shipments::{
    error::Error,
    models::{CreateShipment, EventKind, InputTracking, Shipment, Source, Status, SyncState},
    store::Store,
};
use sqlx::{
    SqlitePool,
    migrate::Migrator,
    sqlite::{SqliteConnectOptions, SqlitePoolOptions},
};
use uuid::Uuid;

#[tokio::test]
async fn completed_override_migration_clears_tracking_errors_and_preserves_registration_and_history()
 {
    #[derive(Debug, PartialEq, Eq, sqlx::FromRow)]
    struct TrackingState {
        override_status: Option<String>,
        carrier_status: Option<String>,
        archived: bool,
        tracker_id: Option<String>,
        registration_started_at: Option<i64>,
        registration_state: String,
        last_error: Option<String>,
        last_error_code: Option<String>,
        last_provider_status: Option<i64>,
        last_provider_error_code: Option<String>,
        attempts: i64,
        next_attempt_at: Option<i64>,
        lease_token: Option<String>,
        lease_until: Option<i64>,
        last_attempt_at: Option<i64>,
        last_success_at: Option<String>,
        revision: i64,
        updated_at: String,
    }
    impl TrackingState {
        async fn load(pool: &SqlitePool, id: Uuid) -> Self {
            sqlx::query_as(
                "SELECT override_status,carrier_status,archived,tracker_id,
                 registration_started_at,registration_state,last_error,
                 last_error_code,last_provider_status,last_provider_error_code,attempts,
                 next_attempt_at,lease_token,lease_until,last_attempt_at,last_success_at,
                 revision,updated_at FROM shipments WHERE id=?",
            )
            .bind(id.to_string())
            .fetch_one(pool)
            .await
            .unwrap()
        }
    }
    #[derive(Debug, PartialEq, Eq, sqlx::FromRow)]
    struct HistoryRow {
        id: i64,
        shipment_id: String,
        kind: String,
        actor_id: String,
        at: String,
        detail: String,
        snapshot: Option<String>,
    }
    #[derive(Debug, PartialEq, Eq, sqlx::FromRow)]
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

    let directory = tempfile::tempdir().unwrap();
    let old_migrations = directory.path().join("before-completed-tracking-errors");
    std::fs::create_dir(&old_migrations).unwrap();
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
            <= 15
        {
            std::fs::copy(entry.path(), old_migrations.join(name)).unwrap();
        }
    }
    let database = directory.path().join("completed-tracking-errors.sqlite3");
    let old = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(
            SqliteConnectOptions::new()
                .filename(&database)
                .create_if_missing(true)
                .foreign_keys(true),
        )
        .await
        .unwrap();
    Migrator::new(old_migrations.as_path())
        .await
        .unwrap()
        .run(&old)
        .await
        .unwrap();
    let mut cases = Vec::new();
    for status in [Status::Delivered, Status::Cancelled, Status::ReturnToSender] {
        for has_tracker in [false, true] {
            cases.push((Some(status), Status::InTransit, has_tracker, true));
        }
        cases.push((None, status, true, false));
    }
    cases.extend([
        (Some(Status::InTransit), Status::Delivered, true, false),
        (
            Some(Status::OutForDelivery),
            Status::Delivered,
            false,
            false,
        ),
        (None, Status::InTransit, false, false),
    ]);
    let mut expected = Vec::new();
    for (index, (manual, carrier, has_tracker, clear)) in cases.into_iter().enumerate() {
        let mut snapshot =
            legacy_shipment(Some(tracking(&format!("COMPLETE{index}123"))), index == 0);
        snapshot.status = manual.unwrap_or(carrier);
        snapshot.carrier_status = Some(carrier);
        snapshot.sync.tracker_id = has_tracker.then(|| format!("trk_completed_{index}"));
        snapshot.sync.registration_state = shipments::models::RegistrationState::Rejected;
        snapshot.sync.last_error = Some("Previous provider failure".into());
        snapshot.status_override = manual.map(|status| shipments::models::StatusOverride {
            status,
            reason: "Manual confirmation".into(),
            actor_id: "owner".into(),
            set_at: "2026-01-02T00:00:00Z".into(),
        });
        sqlx::query(
            "INSERT INTO shipments(id,title,tracking_number,tracking_carrier,archived,
             carrier_status,override_status,override_reason,override_actor,override_at,
             tracker_id,registration_started_at,registration_state,last_error,last_error_code,
             last_provider_status,last_provider_error_code,attempts,next_attempt_at,
             lease_token,lease_until,last_attempt_at,last_success_at)
             VALUES(?,?,?,'usps',?,?,?,?,?,?,?,1700000000,'rejected',
             'Previous provider failure','tracking_carrier_credentials_required',404,
             'CREDENTIALS_NOT_FOUND',3,unixepoch()+600,'in-flight-registration',
             unixepoch()+60,1700000060,'2026-01-01T00:00:00Z')",
        )
        .bind(snapshot.id.to_string())
        .bind(&snapshot.title)
        .bind(&snapshot.tracking.as_ref().unwrap().number)
        .bind(snapshot.archived)
        .bind(serde_json::to_string(&carrier).unwrap())
        .bind(manual.map(|status| serde_json::to_string(&status).unwrap()))
        .bind(manual.map(|_| "Manual confirmation"))
        .bind(manual.map(|_| "owner"))
        .bind(manual.map(|_| "2026-01-02T00:00:00Z"))
        .bind(&snapshot.sync.tracker_id)
        .execute(&old)
        .await
        .unwrap();
        sqlx::query("INSERT INTO events(shipment_id,kind,actor_id,detail,snapshot) VALUES(?,'\"created\"','owner','Original tracking record',?)")
            .bind(snapshot.id.to_string()).bind(serde_json::to_string(&snapshot).unwrap())
            .execute(&old).await.unwrap();
        let mut state = TrackingState::load(&old, snapshot.id).await;
        if clear {
            state.last_error = None;
            state.last_error_code = None;
            state.last_provider_status = None;
            state.last_provider_error_code = None;
            state.attempts = 0;
            state.next_attempt_at = None;
            state.registration_state = if has_tracker { "registered" } else { "pending" }.into();
        }
        expected.push((snapshot.id, state));
    }
    let history: Vec<HistoryRow> = sqlx::query_as("SELECT * FROM events ORDER BY id")
        .fetch_all(&old)
        .await
        .unwrap();
    let state_events: Vec<StateEventRow> = sqlx::query_as("SELECT * FROM state_events ORDER BY id")
        .fetch_all(&old)
        .await
        .unwrap();
    assert_eq!(history.len(), expected.len());
    assert!(!state_events.is_empty());
    old.close().await;

    // Opening again verifies SQLx does not repeat the data repair on restart.
    for _ in 0..2 {
        let store = Store::open(&database).await.unwrap();
        for (id, state) in &expected {
            assert_eq!(&TrackingState::load(store.pool(), *id).await, state);
            assert_eq!(store.events(*id, 0, 100).await.unwrap().len(), 1);
        }
        assert_eq!(
            sqlx::query_as::<_, HistoryRow>("SELECT * FROM events ORDER BY id")
                .fetch_all(store.pool())
                .await
                .unwrap(),
            history
        );
        assert_eq!(
            sqlx::query_as::<_, StateEventRow>("SELECT * FROM state_events ORDER BY id")
                .fetch_all(store.pool())
                .await
                .unwrap(),
            state_events
        );
        store.close().await.unwrap();
    }
}

#[tokio::test]
async fn configuration_migration_stops_setup_failures_but_keeps_outage_retries() {
    let directory = tempfile::tempdir().unwrap();
    let old_migrations = directory.path().join("configuration-old-migrations");
    std::fs::create_dir(&old_migrations).unwrap();
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
            < 13
        {
            std::fs::copy(entry.path(), old_migrations.join(name)).unwrap();
        }
    }
    let database = directory.path().join("configuration-upgrade.sqlite3");
    let old = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(
            SqliteConnectOptions::new()
                .filename(&database)
                .create_if_missing(true)
                .foreign_keys(true),
        )
        .await
        .unwrap();
    Migrator::new(old_migrations.as_path())
        .await
        .unwrap()
        .run(&old)
        .await
        .unwrap();
    let mut ids = Vec::new();
    for (index, status) in [404, 503].into_iter().enumerate() {
        let id = Uuid::now_v7();
        sqlx::query("INSERT INTO shipments(id,title,tracking_number,tracking_carrier,registration_state,last_error,last_error_code,last_provider_status,last_provider_error_code,attempts,next_attempt_at,lease_token,lease_until) VALUES(?,'Setup migration',?,'usps','pending','Setup failure','tracking_carrier_credentials_required',?,'CREDENTIALS_NOT_FOUND',3,unixepoch()+600,'previous-lease',unixepoch()+60)")
            .bind(id.to_string()).bind(format!("CONFIG{index}123")).bind(status).execute(&old).await.unwrap();
        ids.push(id);
    }
    old.close().await;
    let store = Store::open(&database).await.unwrap();
    let configuration = store.get(ids[0]).await.unwrap();
    assert_eq!(
        configuration.sync.registration_state,
        shipments::models::RegistrationState::ConfigurationRequired
    );
    assert_eq!(configuration.sync.attempts, 3);
    assert!(configuration.sync.next_attempt_at.is_none());
    let lease: Option<String> = sqlx::query_scalar("SELECT lease_token FROM shipments WHERE id=?")
        .bind(ids[0].to_string())
        .fetch_one(store.pool())
        .await
        .unwrap();
    assert!(lease.is_none());
    let outage = store.get(ids[1]).await.unwrap();
    assert_eq!(
        outage.sync.registration_state,
        shipments::models::RegistrationState::Pending
    );
    assert!(outage.sync.next_attempt_at.is_some());
    assert_eq!(outage.sync.provider_status, Some(503));
    store.close().await.unwrap();
}

#[tokio::test]
async fn missing_carrier_migration_deletes_incomplete_imports_and_preserves_resolved_records() {
    let directory = tempfile::tempdir().unwrap();
    let old_migrations = directory.path().join("carrier-old-migrations");
    std::fs::create_dir(&old_migrations).unwrap();
    let source = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations");
    for entry in std::fs::read_dir(source).unwrap() {
        let entry = entry.unwrap();
        let name = entry.file_name();
        let name = name.to_str().unwrap();
        if name.split_once('_').unwrap().0.parse::<i64>().unwrap() < 11 {
            std::fs::copy(entry.path(), old_migrations.join(name)).unwrap();
        }
    }
    let database = directory.path().join("carrier-upgrade.sqlite3");
    let legacy = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(
            SqliteConnectOptions::new()
                .filename(&database)
                .create_if_missing(true)
                .foreign_keys(true),
        )
        .await
        .unwrap();
    Migrator::new(old_migrations.as_path())
        .await
        .unwrap()
        .run(&legacy)
        .await
        .unwrap();
    let mut ids = Vec::new();
    for (index, carrier) in [None, Some(""), Some("\u{2003}"), Some("usps"), Some("ups")]
        .into_iter()
        .enumerate()
    {
        let mut snapshot = legacy_shipment(
            Some(InputTracking {
                recipient_url: None,
                number: format!("MIGRATION{index}123"),
                // A resolved record can still have a carrier-less old event.
                carrier: if index == 4 {
                    Some("UPS".parse().unwrap())
                } else {
                    None
                },
            }),
            index == 1,
        );
        snapshot.status = Status::OutForDelivery;
        snapshot.sync.tracker_id = Some(format!("trk_preserve_{index}"));
        snapshot.sync.registration_state = shipments::models::RegistrationState::Registered;
        let number = &snapshot.tracking.as_ref().unwrap().number;
        sqlx::query("INSERT INTO shipments (id,title,tracking_carrier,tracking_number,archived,tracker_id,registration_state) VALUES (?,?,?,?,?,?,'registered')")
            .bind(snapshot.id.to_string()).bind(&snapshot.title).bind(carrier).bind(number)
            .bind(snapshot.archived).bind(&snapshot.sync.tracker_id).execute(&legacy).await.unwrap();
        sqlx::query("INSERT INTO sources(namespace,key,shipment_id) VALUES('upgrade',?,?)")
            .bind(index.to_string())
            .bind(snapshot.id.to_string())
            .execute(&legacy)
            .await
            .unwrap();
        sqlx::query("INSERT INTO events(shipment_id,kind,actor_id,detail,snapshot) VALUES(?,'\"created\"','owner','Old import',?)")
            .bind(snapshot.id.to_string()).bind(serde_json::to_string(&snapshot).unwrap()).execute(&legacy).await.unwrap();
        seed_replay(&legacy, &format!("carrier-{index}"), "legacy", &snapshot).await;
        ids.push(snapshot.id);
    }
    legacy.close().await;
    let store = Store::open(&database).await.unwrap();
    for id in &ids[..3] {
        assert!(matches!(store.get(*id).await, Err(Error::NotFound)));
        let references: (i64, i64, i64, i64) = sqlx::query_as(
            "SELECT (SELECT count(*) FROM sources WHERE shipment_id=?),
             (SELECT count(*) FROM events WHERE shipment_id=?),
             (SELECT count(*) FROM state_events WHERE shipment_id=?),
             (SELECT count(*) FROM idempotency WHERE json_extract(response,'$.id')=?)",
        )
        .bind(id.to_string())
        .bind(id.to_string())
        .bind(id.to_string())
        .bind(id.to_string())
        .fetch_one(store.pool())
        .await
        .unwrap();
        assert_eq!(references, (0, 0, 0, 0));
    }
    for (index, carrier) in [(3, "USPS"), (4, "UPS")] {
        let preserved = store.get(ids[index]).await.unwrap();
        assert_eq!(preserved.tracking.carrier.as_str(), carrier);
        assert_eq!(
            preserved.sync.tracker_id,
            Some(format!("trk_preserve_{index}"))
        );
        let events = store.events(preserved.id, 0, 100).await.unwrap();
        assert_eq!(events.len(), 1);
        let old_carrier = events[0]
            .snapshot
            .as_ref()
            .unwrap()
            .tracking
            .as_ref()
            .unwrap()
            .carrier;
        assert_eq!(old_carrier.is_some(), index == 4);
    }
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM idempotency")
        .fetch_one(store.pool())
        .await
        .unwrap();
    assert_eq!(count, 1);
    for carrier in [None, Some(""), Some("\u{2003}"), Some(" \t\n")] {
        assert!(sqlx::query("INSERT INTO shipments(id,title,tracking_number,tracking_carrier) VALUES(?,'Invalid carrier',?,?)")
            .bind(Uuid::now_v7().to_string()).bind(Uuid::now_v7().to_string()).bind(carrier)
            .execute(store.pool()).await.is_err());
        assert!(
            sqlx::query("UPDATE shipments SET tracking_carrier=? WHERE id=?")
                .bind(carrier)
                .bind(ids[3].to_string())
                .execute(store.pool())
                .await
                .is_err()
        );
    }
    let retry = CreateShipment {
        title: "Corrected import".into(),
        direction: Default::default(),
        merchant: None,
        order_reference: None,
        signature_required: None,
        source: None,
        tracking: InputTracking {
            recipient_url: None,
            number: "MIGRATION0123".into(),
            carrier: Some("USPS".parse().unwrap()),
        },
    };
    assert!(
        store
            .replay_create(&retry, "owner", "carrier-0")
            .await
            .unwrap()
            .is_none()
    );
    let new_record = store
        .create(retry, "owner", "carrier-0", None)
        .await
        .unwrap();
    assert_ne!(new_record.id, ids[0]);
    assert!(
        sqlx::query("PRAGMA foreign_key_check")
            .fetch_all(store.pool())
            .await
            .unwrap()
            .is_empty()
    );
    store.close().await.unwrap();
    let reopened = Store::open(&database).await.unwrap();
    assert_eq!(
        reopened
            .get(ids[3])
            .await
            .unwrap()
            .tracking
            .carrier
            .as_str(),
        "USPS"
    );
    assert!(matches!(reopened.get(ids[0]).await, Err(Error::NotFound)));
    reopened.close().await.unwrap();
}

#[tokio::test]
async fn provider_migration_clarifies_unsupported_errors_without_changing_registration_state() {
    let directory = tempfile::tempdir().unwrap();
    let old_migrations = directory.path().join("old-migrations");
    std::fs::create_dir(&old_migrations).unwrap();
    let source = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations");
    for entry in std::fs::read_dir(source).unwrap() {
        let entry = entry.unwrap();
        let name = entry.file_name();
        let name = name.to_str().unwrap();
        let version: i64 = name.split_once('_').unwrap().0.parse().unwrap();
        if version < 10 {
            std::fs::copy(entry.path(), old_migrations.join(name)).unwrap();
        }
    }
    let database = directory.path().join("upgrade.sqlite3");
    let legacy = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(
            SqliteConnectOptions::new()
                .filename(&database)
                .create_if_missing(true)
                .foreign_keys(true),
        )
        .await
        .unwrap();
    Migrator::new(old_migrations.as_path())
        .await
        .unwrap()
        .run(&legacy)
        .await
        .unwrap();
    let unsupported = Uuid::now_v7();
    let other_error = Uuid::now_v7();
    for (id, number, code, native) in [
        (
            unsupported,
            "9400100000000000000001",
            "tracking_unsupported",
            "TRACKER.NO_CARRIER_FOR_CODE",
        ),
        (
            other_error,
            "9400100000000000000002",
            "tracking_invalid",
            "TRACKER.INVALID_PARAMS",
        ),
    ] {
        sqlx::query(
            "INSERT INTO shipments (id,title,tracking_carrier,tracking_number,registration_state,
             last_error,last_error_code,last_provider_status,last_provider_error_code,
             attempts,next_attempt_at,registration_started_at)
             VALUES (?,'Upgrade parcel','usps',?,'rejected',
             'Old provider error',?,422,?,3,NULL,946684800)",
        )
        .bind(id.to_string())
        .bind(number)
        .bind(code)
        .bind(native)
        .execute(&legacy)
        .await
        .unwrap();
    }
    legacy.close().await;

    // Repeated normal opens prove the migration is durable and does not retry
    // rejected registrations or rewrite unrelated provider errors.
    for _ in 0..2 {
        let store = Store::open(&database).await.unwrap();
        let migrated = store.get(unsupported).await.unwrap();
        assert_eq!(
            migrated.sync.last_error.as_deref(),
            Some(
                "Tracking is unavailable from the configured provider (EasyPost): no supported carrier was found. This does not mean the tracking number is invalid. Check provider support and the selected carrier before changing the number."
            )
        );
        assert_eq!(
            migrated.sync.provider,
            shipments::shipping_api::types::ProviderId::EasyPost
        );
        assert_eq!(
            migrated.sync.registration_state,
            shipments::models::RegistrationState::Rejected
        );
        assert_eq!(migrated.sync.registration_started_at, Some(946684800));
        assert_eq!(
            migrated.sync.error_code.as_deref(),
            Some("tracking_unsupported")
        );
        assert_eq!(
            migrated.sync.provider_error_code.as_deref(),
            Some("TRACKER.NO_CARRIER_FOR_CODE")
        );
        assert_eq!(migrated.sync.provider_status, Some(422));
        assert_eq!(migrated.sync.attempts, 3);
        assert!(migrated.sync.next_attempt_at.is_none());
        assert!(migrated.sync.tracker_id.is_none());
        assert_eq!(migrated.tracking.number, "9400100000000000000001");
        assert_eq!(migrated.tracking.carrier, "USPS".parse().unwrap());
        assert_eq!(
            store
                .get(other_error)
                .await
                .unwrap()
                .sync
                .last_error
                .as_deref(),
            Some("Old provider error")
        );
        store.close().await.unwrap();
    }
}

fn legacy_shipment(
    tracking: Option<InputTracking>,
    archived: bool,
) -> Shipment<Option<InputTracking>> {
    let id = Uuid::now_v7();
    Shipment {
        id,
        direction: Default::default(),
        revision: 1,
        title: "Migration test parcel".into(),
        merchant: None,
        order_reference: None,
        signature_required: None,
        tracking,
        status: Status::Expected,
        carrier_status: None,
        carrier_tracking: Default::default(),
        status_override: None,
        archived,
        created_at: "2026-01-01T00:00:00Z".into(),
        updated_at: "2026-01-01T00:00:00Z".into(),
        sources: vec![Source {
            namespace: "migration-test".into(),
            key: id.to_string(),
        }],
        sync: SyncState {
            provider: Default::default(),
            registration_state: Default::default(),
            registration_started_at: None,
            error_code: None,
            provider_status: None,
            provider_error_code: None,
            tracker_id: None,
            tracking_url: None,
            last_success_at: None,
            last_error: None,
            next_attempt_at: None,
            attempts: 0,
        },
    }
}

fn tracking(number: &str) -> InputTracking {
    InputTracking {
        recipient_url: None,
        number: number.into(),
        carrier: Some("usps".parse().unwrap()),
    }
}

async fn seed_replay(
    pool: &SqlitePool,
    key: &str,
    fingerprint: &str,
    snapshot: &Shipment<Option<InputTracking>>,
) {
    sqlx::query(
        "INSERT INTO idempotency (actor_id, key, fingerprint, response) VALUES (?, ?, ?, ?)",
    )
    .bind("owner")
    .bind(key)
    .bind(fingerprint)
    .bind(serde_json::to_string(snapshot).unwrap())
    .execute(pool)
    .await
    .unwrap();
}

async fn seed_legacy_record(
    pool: &SqlitePool,
    current: &Shipment<Option<InputTracking>>,
    original: &Shipment<Option<InputTracking>>,
    key: &str,
    fingerprint: &str,
) {
    sqlx::query(
        "INSERT INTO shipments (id, title, tracking_carrier, tracking_number, archived) \
         VALUES (?, ?, ?, ?, ?)",
    )
    .bind(current.id.to_string())
    .bind(&current.title)
    .bind(
        current
            .tracking
            .as_ref()
            .and_then(|value| value.carrier.as_ref().map(|carrier| carrier.as_db_key())),
    )
    .bind(current.tracking.as_ref().map(|value| value.number.as_str()))
    .bind(current.archived)
    .execute(pool)
    .await
    .unwrap();
    for source in &current.sources {
        sqlx::query("INSERT INTO sources (namespace, key, shipment_id) VALUES (?, ?, ?)")
            .bind(&source.namespace)
            .bind(&source.key)
            .bind(current.id.to_string())
            .execute(pool)
            .await
            .unwrap();
    }
    sqlx::query(
        "INSERT INTO events (shipment_id, kind, actor_id, detail, snapshot) \
         VALUES (?, ?, ?, ?, ?)",
    )
    .bind(current.id.to_string())
    .bind(serde_json::to_string(&EventKind::Created).unwrap())
    .bind("owner")
    .bind("Legacy shipment created")
    .bind(serde_json::to_string(original).unwrap())
    .execute(pool)
    .await
    .unwrap();
    seed_replay(pool, key, fingerprint, original).await;
}

#[tokio::test]
async fn required_tracking_migration_preserves_valid_records_and_historical_snapshots() {
    let dir = tempfile::tempdir().unwrap();
    let migrations = dir.path().join("legacy-migrations");
    std::fs::create_dir(&migrations).unwrap();
    for filename in ["0001_registry.sql", "0002_agent_credentials.sql"] {
        std::fs::copy(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("migrations")
                .join(filename),
            migrations.join(filename),
        )
        .unwrap();
    }
    let database = dir.path().join("legacy.sqlite3");
    let legacy = SqlitePoolOptions::new()
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
        .run(&legacy)
        .await
        .unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, i64>("PRAGMA foreign_keys")
            .fetch_one(&legacy)
            .await
            .unwrap(),
        1
    );

    let missing = [
        legacy_shipment(None, false),
        legacy_shipment(Some(tracking(" \t\r\n")), false),
        legacy_shipment(Some(tracking("")), true),
        legacy_shipment(Some(tracking("\u{a0}\u{2003}")), false),
    ];
    for (index, current) in missing.iter().enumerate() {
        let mut original = current.clone();
        if index == 0 {
            // Its valid old create response must not resurrect the deleted row.
            original.tracking = Some(tracking("REMOVED123"));
        }
        seed_legacy_record(
            &legacy,
            current,
            &original,
            &format!("removed-{index}"),
            "legacy-fingerprint",
        )
        .await;
    }

    let mut valid = legacy_shipment(Some(tracking("VALID123")), false);
    valid.sources[0].key = "valid-source".into();
    let valid_input = CreateShipment {
        direction: shipments::models::Direction::Inbound,
        title: valid.title.clone(),
        merchant: valid.merchant.clone(),
        order_reference: valid.order_reference.clone(),
        signature_required: None,
        tracking: valid.tracking.clone().unwrap(),
        source: Some(valid.sources[0].clone()),
    };
    // SHA-256 of the 0.3.1 CreateShipment: lowercase carrier and no signature
    // field. New API models must still replay this old record.
    let fingerprint = "b856d5d7b7113242a09bcef1afac65d647582fbb3755e04adfb7c2696dd48ebb";
    seed_legacy_record(&legacy, &valid, &valid, "valid-create", fingerprint).await;

    let now_tracked = legacy_shipment(Some(tracking("NOWTRACKED123")), false);
    let mut originally_missing = now_tracked.clone();
    originally_missing.tracking = None;
    seed_legacy_record(
        &legacy,
        &now_tracked,
        &originally_missing,
        "retained-old-create",
        "legacy-fingerprint",
    )
    .await;
    let mut blank_replay = originally_missing.clone();
    blank_replay.tracking = Some(tracking("\u{a0}\u{2003}"));
    seed_replay(
        &legacy,
        "retained-blank-create",
        "legacy-fingerprint",
        &blank_replay,
    )
    .await;
    let agent_id = Uuid::now_v7();
    let digest: [u8; 32] = Sha256::digest("disposable-migration-test-credential").into();
    sqlx::query("INSERT INTO agent_credentials (id, name, token_digest) VALUES (?, ?, ?)")
        .bind(agent_id.to_string())
        .bind("Migration test agent")
        .bind(digest.as_slice())
        .execute(&legacy)
        .await
        .unwrap();
    legacy.close().await;

    // The first open performs 0003; both later opens must be harmless no-ops.
    for opening in 0..3 {
        let store = Store::open(&database).await.unwrap();
        let mut ids = sqlx::query_scalar::<_, String>("SELECT id FROM shipments ORDER BY id")
            .fetch_all(store.pool())
            .await
            .unwrap();
        ids.sort();
        let mut expected = vec![valid.id.to_string(), now_tracked.id.to_string()];
        expected.sort();
        assert_eq!(ids, expected);
        for removed in &missing {
            assert!(matches!(store.get(removed.id).await, Err(Error::NotFound)));
            assert!(matches!(
                store.events(removed.id, 0, 100).await,
                Err(Error::NotFound)
            ));
            let children = sqlx::query_as::<_, (i64, i64)>(
                "SELECT (SELECT count(*) FROM sources WHERE shipment_id = ?), \
                 (SELECT count(*) FROM events WHERE shipment_id = ?)",
            )
            .bind(removed.id.to_string())
            .bind(removed.id.to_string())
            .fetch_one(store.pool())
            .await
            .unwrap();
            assert_eq!(children, (0, 0));
        }
        for retained in [&valid, &now_tracked] {
            let current = store.get(retained.id).await.unwrap();
            assert_eq!(
                Some(InputTracking::from(current.tracking)),
                retained.tracking
            );
            assert_eq!(current.sources, retained.sources);
            assert!(!current.archived);
        }
        let history = store.events(now_tracked.id, 0, 100).await.unwrap();
        assert_eq!(history.len(), 1);
        let snapshot = history[0].snapshot.as_ref().unwrap();
        assert_eq!(snapshot.id, now_tracked.id);
        assert!(snapshot.tracking.is_none());
        assert!(snapshot.sync.tracking_url.is_none());
        let valid_history = store.events(valid.id, 0, 100).await.unwrap();
        assert_eq!(valid_history.len(), 1);
        assert_eq!(
            valid_history[0]
                .snapshot
                .as_ref()
                .unwrap()
                .tracking
                .clone()
                .map(|t| InputTracking {
                    carrier: t.carrier,
                    number: t.number,
                    recipient_url: t.recipient_url
                }),
            valid.tracking
        );
        let keys = sqlx::query_scalar::<_, String>("SELECT key FROM idempotency ORDER BY key")
            .fetch_all(store.pool())
            .await
            .unwrap();
        assert_eq!(keys, ["valid-create"]);
        let replay = store
            .replay_create(&valid_input, "owner", "valid-create")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(replay.id, valid.id);
        assert_eq!(replay.revision, 1);
        assert_eq!(Some(InputTracking::from(replay.tracking)), valid.tracking);
        assert_eq!(store.agent_identity(&digest).await.unwrap(), Some(agent_id));
        let agents = store.agents().await.unwrap();
        assert_eq!(agents.items.len(), 1);
        assert_eq!(agents.items[0].name, "Migration test agent");
        assert!(
            sqlx::query("PRAGMA foreign_key_check")
                .fetch_all(store.pool())
                .await
                .unwrap()
                .is_empty()
        );

        if opening == 0 {
            for number in [None, Some(""), Some(" \t\r\n"), Some("\u{a0}\u{2003}")] {
                let inserted = sqlx::query(
                    "INSERT INTO shipments (id, title, tracking_number, tracking_carrier) VALUES (?, ?, ?, 'usps')",
                )
                .bind(Uuid::now_v7().to_string())
                .bind("Rejected test parcel")
                .bind(number)
                .execute(store.pool())
                .await
                .unwrap_err();
                assert!(
                    inserted
                        .as_database_error()
                        .unwrap()
                        .message()
                        .contains("tracking number is required")
                );
                let updated = sqlx::query("UPDATE shipments SET tracking_number = ? WHERE id = ?")
                    .bind(number)
                    .bind(valid.id.to_string())
                    .execute(store.pool())
                    .await
                    .unwrap_err();
                assert!(
                    updated
                        .as_database_error()
                        .unwrap()
                        .message()
                        .contains("tracking number is required")
                );
            }
        }
        store.close().await.unwrap();
    }
}

#[tokio::test]
async fn carrier_history_upgrade_queues_one_enrichment_without_breaking_retry_or_archive_state() {
    let directory = tempfile::tempdir().unwrap();
    let migrations = directory.path().join("old-migrations");
    std::fs::create_dir(&migrations).unwrap();
    for filename in [
        "0001_registry.sql",
        "0002_agent_credentials.sql",
        "0003_require_tracking.sql",
        "0004_smart_polling.sql",
    ] {
        std::fs::copy(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("migrations")
                .join(filename),
            migrations.join(filename),
        )
        .unwrap();
    }
    let path = directory.path().join("upgrade.sqlite3");
    let legacy = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(
            SqliteConnectOptions::new()
                .filename(&path)
                .create_if_missing(true)
                .foreign_keys(true),
        )
        .await
        .unwrap();
    Migrator::new(migrations.as_path())
        .await
        .unwrap()
        .run(&legacy)
        .await
        .unwrap();
    let before: i64 = sqlx::query_scalar("SELECT unixepoch()")
        .fetch_one(&legacy)
        .await
        .unwrap();
    let mut expected = Vec::new();
    for (archived, terminal, manual_terminal, registered, failed, previous_due, queued) in [
        (false, true, false, true, false, None, true),
        (false, false, true, true, false, None, false),
        (true, true, false, true, false, None, false),
        (true, false, false, true, false, Some(before + 1800), false),
        (false, false, false, true, true, Some(before + 900), false),
        (false, false, false, true, true, Some(before - 10), true),
        (false, false, false, true, false, Some(before + 1800), true),
        (false, false, false, false, false, Some(before + 1800), true),
        (false, false, false, true, false, Some(before - 10), true),
    ] {
        let id = Uuid::now_v7();
        sqlx::query("INSERT INTO shipments(id,title,tracking_number,tracking_carrier,archived,carrier_status,override_status,tracker_id,last_error,next_attempt_at) VALUES(?,?,?,?,?,?,?,?,?,?)")
            .bind(id.to_string()).bind("Upgrade test parcel").bind(format!("UPGRADE{id}"))
            .bind("usps").bind(archived).bind(if terminal { "\"delivered\"" } else { "\"in_transit\"" })
            .bind(manual_terminal.then_some("\"delivered\""))
            .bind(registered.then_some("trk_upgrade"))
            .bind(failed.then_some("Temporary provider error")).bind(previous_due)
            .execute(&legacy).await.unwrap();
        expected.push((id, previous_due, queued));
    }
    legacy.close().await;
    let upgraded = Store::open(&path).await.unwrap();
    let after: i64 = sqlx::query_scalar("SELECT unixepoch()")
        .fetch_one(upgraded.pool())
        .await
        .unwrap();
    for (id, previous_due, queued) in &expected {
        let actual: Option<i64> =
            sqlx::query_scalar("SELECT next_attempt_at FROM shipments WHERE id=?")
                .bind(id.to_string())
                .fetch_one(upgraded.pool())
                .await
                .unwrap();
        if *queued {
            assert!((before..=after).contains(&actual.unwrap()));
        } else {
            assert_eq!(actual, *previous_due);
        }
        let shipment = upgraded.get(*id).await.unwrap();
        assert!(shipment.carrier_tracking.events.is_empty());
        assert!(shipment.signature_required.is_none());
    }
    let delivered_id = expected[0].0;
    sqlx::query("UPDATE shipments SET next_attempt_at=NULL WHERE id=?")
        .bind(delivered_id.to_string())
        .execute(upgraded.pool())
        .await
        .unwrap();
    upgraded.close().await.unwrap();
    let reopened = Store::open(&path).await.unwrap();
    assert!(
        reopened
            .get(delivered_id)
            .await
            .unwrap()
            .sync
            .next_attempt_at
            .is_none(),
        "enrichment must only be queued once during the migration"
    );
    reopened.close().await.unwrap();
}

#[tokio::test]
async fn direction_upgrade_marks_existing_and_historical_shipments_inbound() {
    use shipments::models::{Direction, Patch, UpdateShipment};
    let directory = tempfile::tempdir().unwrap();
    let migrations = directory.path().join("before-direction");
    std::fs::create_dir(&migrations).unwrap();
    for filename in [
        "0001_registry.sql",
        "0002_agent_credentials.sql",
        "0003_require_tracking.sql",
        "0004_smart_polling.sql",
        "0005_carrier_history.sql",
        "0006_delivery_sort.sql",
    ] {
        std::fs::copy(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("migrations")
                .join(filename),
            migrations.join(filename),
        )
        .unwrap();
    }
    let path = directory.path().join("direction-upgrade.sqlite3");
    let legacy = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(
            SqliteConnectOptions::new()
                .filename(&path)
                .create_if_missing(true)
                .foreign_keys(true),
        )
        .await
        .unwrap();
    Migrator::new(migrations.as_path())
        .await
        .unwrap()
        .run(&legacy)
        .await
        .unwrap();
    let mut ids = Vec::new();
    for archived in [false, true] {
        let shipment = legacy_shipment(
            Some(tracking(if archived {
                "ARCHIVED-DIRECTION"
            } else {
                "ACTIVE-DIRECTION"
            })),
            archived,
        );
        sqlx::query("INSERT INTO shipments(id,title,tracking_number,tracking_carrier,archived)VALUES(?,?,?,?,?)")
            .bind(shipment.id.to_string()).bind(&shipment.title).bind(&shipment.tracking.as_ref().unwrap().number).bind("usps").bind(archived).execute(&legacy).await.unwrap();
        // The older serialized snapshot has no direction field at all.
        let snapshot = serde_json::to_string(&shipment)
            .unwrap()
            .replace("\"direction\":\"inbound\",", "");
        sqlx::query("INSERT INTO events(shipment_id,kind,actor_id,detail,snapshot)VALUES(?,'\"created\"','test','Legacy create',?)")
            .bind(shipment.id.to_string()).bind(snapshot).execute(&legacy).await.unwrap();
        ids.push(shipment.id);
    }
    legacy.close().await;
    let upgraded = Store::open(&path).await.unwrap();
    for id in &ids {
        assert_eq!(
            upgraded.get(*id).await.unwrap().direction,
            Direction::Inbound
        );
        assert_eq!(
            upgraded.events(*id, 0, 10).await.unwrap()[0]
                .snapshot
                .as_ref()
                .unwrap()
                .direction,
            Direction::Inbound
        );
    }
    let current = upgraded.get(ids[0]).await.unwrap();
    upgraded
        .update(
            current.id,
            current.revision,
            UpdateShipment {
                direction: Patch {
                    value: Some(Some(Direction::Outbound)),
                },
                ..Default::default()
            },
            "owner",
            None,
        )
        .await
        .unwrap();
    assert!(
        sqlx::query("UPDATE shipments SET direction='sideways' WHERE id=?")
            .bind(current.id.to_string())
            .execute(upgraded.pool())
            .await
            .is_err()
    );
    upgraded.close().await.unwrap();
    let reopened = Store::open(&path).await.unwrap();
    assert_eq!(
        reopened.get(ids[0]).await.unwrap().direction,
        Direction::Outbound
    );
    assert_eq!(
        reopened.get(ids[1]).await.unwrap().direction,
        Direction::Inbound
    );
    reopened.close().await.unwrap();
}

#[tokio::test]
async fn pending_registration_upgrade_preserves_known_trackers_attempts_and_retry_advice() {
    use shipments::models::RegistrationState;
    let directory = tempfile::tempdir().unwrap();
    let migrations = directory.path().join("before-pending");
    std::fs::create_dir(&migrations).unwrap();
    for filename in [
        "0001_registry.sql",
        "0002_agent_credentials.sql",
        "0003_require_tracking.sql",
        "0004_smart_polling.sql",
        "0005_carrier_history.sql",
        "0006_delivery_sort.sql",
        "0007_direction.sql",
    ] {
        std::fs::copy(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("migrations")
                .join(filename),
            migrations.join(filename),
        )
        .unwrap();
    }
    let path = directory.path().join("pending-upgrade.sqlite3");
    let legacy = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(
            SqliteConnectOptions::new()
                .filename(&path)
                .create_if_missing(true)
                .foreign_keys(true),
        )
        .await
        .unwrap();
    Migrator::new(migrations.as_path())
        .await
        .unwrap()
        .run(&legacy)
        .await
        .unwrap();
    let now: i64 = sqlx::query_scalar("SELECT unixepoch()")
        .fetch_one(&legacy)
        .await
        .unwrap();
    let mut cases = Vec::new();
    for (registered, archived, terminal, failed, attempted, next) in [
        (true, false, false, false, Some(123), Some(now + 1800)),
        (true, true, true, false, None, None),
        (false, false, false, false, None, None),
        (false, false, false, true, Some(123), Some(now + 900)),
        (false, false, true, false, Some(123), None),
        (false, true, false, false, Some(123), None),
    ] {
        let id = Uuid::now_v7();
        sqlx::query("INSERT INTO shipments(id,title,tracking_number,tracking_carrier,tracker_id,archived,carrier_status,last_error,last_attempt_at,next_attempt_at)VALUES(?,?,?,?,?,?,?,?,?,?)")
            .bind(id.to_string()).bind("Pending upgrade parcel").bind(format!("PENDING{id}")).bind("usps")
            .bind(registered.then_some("trk_existing")).bind(archived)
            .bind(if terminal { "\"delivered\"" } else { "\"unknown\"" })
            .bind(failed.then_some("Provider unavailable")).bind(attempted).bind(next)
            .execute(&legacy).await.unwrap();
        cases.push((id, registered, archived, terminal, failed, attempted, next));
    }
    legacy.close().await;
    let upgraded = Store::open(&path).await.unwrap();
    for (id, registered, archived, terminal, failed, attempted, previous_next) in cases {
        let shipment = upgraded.get(id).await.unwrap();
        assert_eq!(
            shipment.sync.registration_state,
            if registered {
                RegistrationState::Registered
            } else {
                RegistrationState::Pending
            }
        );
        assert_eq!(
            shipment.sync.registration_started_at,
            if registered { None } else { attempted }
        );
        assert!(shipment.sync.error_code.is_none());
        assert!(shipment.sync.provider_status.is_none());
        assert!(shipment.sync.provider_error_code.is_none());
        if registered || archived || terminal || failed {
            assert_eq!(shipment.sync.next_attempt_at, previous_next);
        } else {
            assert!(shipment.sync.next_attempt_at.unwrap() >= now);
        }
    }
    upgraded.close().await.unwrap();
}
