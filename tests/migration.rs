use sha2::{Digest, Sha256};
use shipments::{
    error::Error,
    models::{CreateShipment, EventKind, Shipment, Source, Status, SyncState, Tracking},
    store::Store,
};
use sqlx::{
    SqlitePool,
    migrate::Migrator,
    sqlite::{SqliteConnectOptions, SqlitePoolOptions},
};
use uuid::Uuid;

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
        assert_eq!(migrated.tracking.carrier, Some("USPS".parse().unwrap()));
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

fn legacy_shipment(tracking: Option<Tracking>, archived: bool) -> Shipment<Option<Tracking>> {
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

fn tracking(number: &str) -> Tracking {
    Tracking {
        number: number.into(),
        carrier: Some("usps".parse().unwrap()),
    }
}

async fn seed_replay(
    pool: &SqlitePool,
    key: &str,
    fingerprint: &str,
    snapshot: &Shipment<Option<Tracking>>,
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
    current: &Shipment<Option<Tracking>>,
    original: &Shipment<Option<Tracking>>,
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
    let fingerprint = format!(
        "{:x}",
        // Exact 0.3.1 CreateShipment serialization: lowercase carrier and no
        // signature field. New API models must still replay this old record.
        Sha256::digest(br#"{"title":"Migration test parcel","merchant":null,"order_reference":null,"tracking":{"carrier":"usps","number":"VALID123"},"source":{"namespace":"migration-test","key":"valid-source"}}"#)
    );
    seed_legacy_record(&legacy, &valid, &valid, "valid-create", &fingerprint).await;

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
            assert_eq!(Some(current.tracking), retained.tracking);
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
            valid_history[0].snapshot.as_ref().unwrap().tracking,
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
        assert_eq!(Some(replay.tracking), valid.tracking);
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
                    "INSERT INTO shipments (id, title, tracking_number) VALUES (?, ?, ?)",
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
        (false, false, true, true, false, None, true),
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
