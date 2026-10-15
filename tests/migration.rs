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

fn legacy_shipment(tracking: Option<Tracking>, archived: bool) -> Shipment<Option<Tracking>> {
    let id = Uuid::now_v7();
    Shipment {
        id,
        revision: 1,
        title: "Migration test parcel".into(),
        merchant: None,
        order_reference: None,
        tracking,
        status: Status::Expected,
        carrier_status: None,
        status_override: None,
        archived,
        created_at: "2026-01-01T00:00:00Z".into(),
        updated_at: "2026-01-01T00:00:00Z".into(),
        sources: vec![Source {
            namespace: "migration-test".into(),
            key: id.to_string(),
        }],
        sync: SyncState {
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
        carrier: Some("usps".into()),
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
            .and_then(|value| value.carrier.as_deref()),
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

    let valid = legacy_shipment(Some(tracking("VALID123")), false);
    let valid_input = CreateShipment {
        title: valid.title.clone(),
        merchant: valid.merchant.clone(),
        order_reference: valid.order_reference.clone(),
        tracking: valid.tracking.clone().unwrap(),
        source: Some(valid.sources[0].clone()),
    };
    let fingerprint = format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&valid_input).unwrap())
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
