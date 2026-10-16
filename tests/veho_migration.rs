//! Veho provider migration preserves identity and schedules safe reconciliation.

use shipments::{
    carrier::Carrier,
    models::{CarrierTracking, RegistrationState, Source, Status},
    shipping_api::types::ProviderId,
    store::Store,
};
use sqlx::{
    SqlitePool,
    migrate::Migrator,
    sqlite::{SqliteConnectOptions, SqlitePoolOptions},
};
use uuid::Uuid;

#[derive(sqlx::FromRow, Debug, PartialEq, Eq)]
struct Ownership {
    tracking_generation: i64,
    lease_token: Option<String>,
    lease_until: Option<i64>,
    last_attempt_at: Option<i64>,
}

async fn seed(
    pool: &SqlitePool,
    index: usize,
    carrier: &str,
    provider: ProviderId,
    status: Status,
    archived: bool,
    has_tracker: bool,
) -> Uuid {
    let id = Uuid::now_v7();
    let history = CarrierTracking {
        estimated_delivery_at: Some("2026-10-15T19:00:00Z".into()),
        ..Default::default()
    };
    sqlx::query("INSERT INTO shipments(id,title,tracking_number,tracking_carrier,tracking_provider,tracking_generation,revision,carrier_status,carrier_tracking,archived,tracker_id,tracking_url,registration_state,registration_started_at,last_success_at,last_attempt_at,last_error,last_error_code,last_provider_status,last_provider_error_code,attempts,next_attempt_at,lease_token,lease_until) VALUES(?,'Veho migration fixture',?,?,?,7,3,?,?,?,?,?,'rejected',123,'2026-10-14T19:00:00Z',456,'Old credential error','tracking_carrier_credentials_required',404,'CREDENTIALS_NOT_FOUND',5,789,'previous-worker-lease',unixepoch()+60)")
        .bind(id.to_string()).bind(format!("MIGRATION{index}123456")).bind(carrier)
        .bind(provider.as_str()).bind(serde_json::to_string(&status).unwrap())
        .bind(serde_json::to_string(&history).unwrap()).bind(archived)
        .bind(has_tracker.then(|| format!("old-tracker-{index}")))
        .bind("https://tracking.example.test/old-provider")
        .execute(pool).await.unwrap();
    sqlx::query("INSERT INTO sources(namespace,key,shipment_id) VALUES('migration',?,?)")
        .bind(format!("source-{index}"))
        .bind(id.to_string())
        .execute(pool)
        .await
        .unwrap();
    id
}

async fn ownership(store: &Store, id: Uuid) -> Ownership {
    sqlx::query_as("SELECT tracking_generation,lease_token,lease_until,last_attempt_at FROM shipments WHERE id=?")
        .bind(id.to_string()).fetch_one(store.pool()).await.unwrap()
}

#[tokio::test]
async fn veho_upgrade_reroutes_easypost_records_and_preserves_history_and_ownership_audit() {
    let directory = tempfile::tempdir().unwrap();
    let old_migrations = directory.path().join("old-migrations");
    std::fs::create_dir(&old_migrations).unwrap();
    for entry in
        std::fs::read_dir(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations"))
            .unwrap()
    {
        let entry = entry.unwrap();
        let name = entry.file_name();
        let version = name
            .to_str()
            .unwrap()
            .split_once('_')
            .unwrap()
            .0
            .parse::<u32>()
            .unwrap();
        if version < 15 {
            std::fs::copy(entry.path(), old_migrations.join(name)).unwrap();
        }
    }
    let database = directory.path().join("shipments.sqlite3");
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
    let mut moved = Vec::new();
    for (index, carrier, status, archived, has_tracker) in [
        (0, "veho", Status::OutForDelivery, false, true),
        (1, "Veho", Status::Expected, false, false),
        (2, "VEHO", Status::InTransit, true, true),
        (3, "veho", Status::Delivered, false, true),
    ] {
        let id = seed(
            &old,
            index,
            carrier,
            ProviderId::EasyPost,
            status,
            archived,
            has_tracker,
        )
        .await;
        moved.push((id, index, status, archived, has_tracker));
    }
    let existing_aftership = seed(
        &old,
        4,
        "veho",
        ProviderId::AfterShip,
        Status::OutForDelivery,
        false,
        true,
    )
    .await;
    let usps = seed(
        &old,
        5,
        "usps",
        ProviderId::EasyPost,
        Status::OutForDelivery,
        false,
        true,
    )
    .await;
    old.close().await;
    let store = Store::open(&database).await.unwrap();
    for (id, index, status, archived, has_tracker) in &moved {
        let shipment = store.get(*id).await.unwrap();
        assert_eq!(shipment.tracking.carrier, Carrier::Veho);
        assert_eq!(shipment.tracking.number, format!("MIGRATION{index}123456"));
        assert_eq!(shipment.sync.provider, ProviderId::AfterShip);
        assert_eq!(shipment.revision, 4);
        assert_eq!(shipment.status, *status);
        assert_eq!(shipment.archived, *archived);
        assert_eq!(shipment.sync.registration_state, RegistrationState::Pending);
        assert!(shipment.sync.tracker_id.is_none());
        assert!(shipment.sync.tracking_url.is_none());
        assert!(shipment.sync.registration_started_at.is_none());
        assert!(shipment.sync.last_success_at.is_none());
        assert!(shipment.sync.last_error.is_none());
        assert!(shipment.sync.error_code.is_none());
        assert!(shipment.sync.provider_status.is_none());
        assert!(shipment.sync.provider_error_code.is_none());
        assert_eq!(shipment.sync.attempts, 0);
        assert_eq!(
            shipment.sync.next_attempt_at.is_some(),
            !archived && !status.is_terminal()
        );
        assert_eq!(
            shipment.carrier_tracking.estimated_delivery_at.as_deref(),
            Some("2026-10-15T19:00:00Z")
        );
        assert_eq!(
            shipment.sources,
            [Source {
                namespace: "migration".into(),
                key: format!("source-{index}"),
            }]
        );
        assert_eq!(
            ownership(&store, *id).await,
            Ownership {
                tracking_generation: 8,
                lease_token: None,
                lease_until: None,
                last_attempt_at: None,
            }
        );
        let events = store.events(*id, 0, 100).await.unwrap();
        assert_eq!(events.len(), usize::from(*has_tracker));
        if *has_tracker {
            assert_eq!(events[0].actor_id, "system");
            assert_eq!(
                events[0].detail,
                format!(
                    "Tracking moved to AfterShip; previous EasyPost tracker: old-tracker-{index}"
                )
            );
        }
    }
    for (id, provider) in [
        (existing_aftership, ProviderId::AfterShip),
        (usps, ProviderId::EasyPost),
    ] {
        let shipment = store.get(id).await.unwrap();
        assert_eq!(shipment.sync.provider, provider);
        assert_eq!(shipment.revision, 3);
        assert!(shipment.sync.tracker_id.is_some());
        assert_eq!(shipment.sync.attempts, 5);
        assert_eq!(ownership(&store, id).await.tracking_generation, 7);
        assert!(store.events(id, 0, 100).await.unwrap().is_empty());
    }
    assert!(
        sqlx::query("PRAGMA foreign_key_check")
            .fetch_all(store.pool())
            .await
            .unwrap()
            .is_empty()
    );
    store.close().await.unwrap();
    let reopened = Store::open(&database).await.unwrap();
    for (id, _, _, _, has_tracker) in moved {
        assert_eq!(
            reopened.get(id).await.unwrap().sync.provider,
            ProviderId::AfterShip
        );
        assert_eq!(ownership(&reopened, id).await.tracking_generation, 8);
        assert_eq!(
            reopened.events(id, 0, 100).await.unwrap().len(),
            usize::from(has_tracker)
        );
    }
    reopened.close().await.unwrap();
}
