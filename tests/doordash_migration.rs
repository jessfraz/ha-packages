//! Upgrade and database constraints preserve recipient-link ownership.

use std::path::Path;

use sha2::{Digest, Sha256};
use shipments::{
    carrier::Carrier,
    error::Error,
    models::{
        CarrierTracking, CreateShipment, Direction, InputTracking, RegistrationState, Shipment,
        Source, Status, SyncState, Tracking,
    },
    shipping_api::types::ProviderId,
    store::Store,
};
use sqlx::{
    SqlitePool,
    migrate::Migrator,
    sqlite::{SqliteConnectOptions, SqlitePoolOptions},
};
use uuid::Uuid;

const LINK: &str = "https://www.doordash.com/orders/drive?urlCode=migration-recipient-code-123";

fn legacy_input(index: usize, carrier: Carrier) -> CreateShipment {
    CreateShipment {
        title: "Migration fixture parcel".into(),
        direction: Direction::Inbound,
        merchant: None,
        order_reference: None,
        signature_required: None,
        tracking: InputTracking {
            number: format!("LEGACY{index}123456"),
            carrier: Some(carrier),
            recipient_url: None,
        },
        source: Some(Source {
            namespace: "migration".into(),
            key: format!("source-{index}"),
        }),
    }
}

fn admission_fingerprint(input: &CreateShipment) -> String {
    Sha256::digest(serde_json::to_vec(input).unwrap())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

async fn legacy_database(path: &Path, migrations: &Path) -> SqlitePool {
    std::fs::create_dir(migrations).unwrap();
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
            .parse::<u32>()
            .unwrap();
        if version < 14 {
            std::fs::copy(entry.path(), migrations.join(name)).unwrap();
        }
    }
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(
            SqliteConnectOptions::new()
                .filename(path)
                .create_if_missing(true)
                .foreign_keys(true),
        )
        .await
        .unwrap();
    Migrator::new(migrations)
        .await
        .unwrap()
        .run(&pool)
        .await
        .unwrap();
    pool
}

async fn seed(pool: &SqlitePool, index: usize, carrier: &str, archived: bool) -> Uuid {
    let id = Uuid::now_v7();
    let snapshot: Shipment = Shipment {
        id,
        revision: 1,
        title: "Migration fixture parcel".into(),
        direction: Direction::Inbound,
        merchant: None,
        order_reference: None,
        signature_required: None,
        tracking: Tracking {
            number: format!("LEGACY{index}123456"),
            carrier: carrier.parse().unwrap(),
            recipient_url: None,
        },
        status: Status::OutForDelivery,
        carrier_status: Some(Status::OutForDelivery),
        carrier_tracking: CarrierTracking::default(),
        status_override: None,
        archived,
        created_at: "2026-10-15T10:00:00Z".into(),
        updated_at: "2026-10-15T10:00:00Z".into(),
        sources: vec![Source {
            namespace: "migration".into(),
            key: format!("source-{index}"),
        }],
        sync: SyncState {
            provider: Default::default(),
            registration_state: RegistrationState::Registered,
            registration_started_at: None,
            error_code: None,
            provider_status: None,
            provider_error_code: None,
            tracker_id: Some(format!("legacy-tracker-{index}")),
            tracking_url: None,
            last_success_at: None,
            last_error: None,
            next_attempt_at: None,
            attempts: 0,
        },
    };
    let serialized = serde_json::to_string(&snapshot).unwrap();
    sqlx::query("INSERT INTO shipments(id,title,tracking_number,tracking_carrier,carrier_status,archived,tracker_id,registration_state) VALUES(?,?,?,?,'\"out_for_delivery\"',?,?,'registered')")
        .bind(id.to_string()).bind(&snapshot.title).bind(&snapshot.tracking.number).bind(carrier)
        .bind(archived).bind(&snapshot.sync.tracker_id).execute(pool).await.unwrap();
    sqlx::query("INSERT INTO sources(namespace,key,shipment_id) VALUES('migration',?,?)")
        .bind(format!("source-{index}"))
        .bind(id.to_string())
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO events(shipment_id,kind,actor_id,detail,snapshot) VALUES(?,'\"created\"','owner','Old recipient import',?)")
        .bind(id.to_string()).bind(&serialized).execute(pool).await.unwrap();
    sqlx::query("INSERT INTO idempotency(actor_id,key,fingerprint,response) VALUES('owner',?,'legacy-fingerprint',?)")
        .bind(format!("legacy-{index}")).bind(serialized).execute(pool).await.unwrap();
    sqlx::query(
        "INSERT INTO admission_requests(actor_id,operation_key,fingerprint) VALUES('owner',?,?)",
    )
    .bind(format!("create:legacy-{index}"))
    .bind(admission_fingerprint(&legacy_input(
        index,
        carrier.parse().unwrap(),
    )))
    .execute(pool)
    .await
    .unwrap();
    id
}

#[tokio::test]
async fn upgrade_deletes_only_doordash_without_original_links_and_cleans_dependent_records() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("legacy.sqlite3");
    let legacy = legacy_database(&path, &directory.path().join("old-migrations")).await;
    let mut removed = Vec::new();
    for (index, carrier, archived) in [
        (0, "DoorDash", false),
        (1, "doordash", false),
        (2, "DOORDASH", true),
    ] {
        removed.push(seed(&legacy, index, carrier, archived).await);
    }
    let mut preserved = Vec::new();
    for (index, carrier) in [(3, "USPS"), (4, "SpeedX"), (5, "Sway")] {
        preserved.push((seed(&legacy, index, carrier, false).await, carrier));
    }
    // Replay records can outlive the original import independently of a row.
    sqlx::query("INSERT INTO idempotency(actor_id,key,fingerprint,response) SELECT 'owner','orphan-doordash-replay','legacy-fingerprint',json_set(response,'$.id',?) FROM idempotency WHERE actor_id='owner' AND key='legacy-0'")
        .bind(Uuid::now_v7().to_string()).execute(&legacy).await.unwrap();
    sqlx::query("INSERT INTO admission_requests(actor_id,operation_key,fingerprint) SELECT actor_id,'create:orphan-doordash-replay',fingerprint FROM admission_requests WHERE actor_id='owner' AND operation_key='create:legacy-0'")
        .execute(&legacy).await.unwrap();
    for (actor, key, fingerprint) in [
        ("owner", "legacy-0", "unprefixed-operation-control"),
        ("other-owner", "create:legacy-0", "other-actor-control"),
        ("owner", "create:unrelated", "unrelated-request-control"),
    ] {
        sqlx::query(
            "INSERT INTO admission_requests(actor_id,operation_key,fingerprint) VALUES(?,?,?)",
        )
        .bind(actor)
        .bind(key)
        .bind(fingerprint)
        .execute(&legacy)
        .await
        .unwrap();
    }
    legacy.close().await;
    let upgraded = Store::open(&path).await.unwrap();
    for id in &removed {
        assert!(matches!(upgraded.get(*id).await, Err(Error::NotFound)));
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
        .fetch_one(upgraded.pool())
        .await
        .unwrap();
        assert_eq!(references, (0, 0, 0, 0));
    }
    for (id, carrier) in &preserved {
        let shipment = upgraded.get(*id).await.unwrap();
        assert_eq!(shipment.tracking.carrier.as_str(), *carrier);
        assert!(shipment.tracking.recipient_url.is_none());
        let events = upgraded.events(*id, 0, 100).await.unwrap();
        assert!(
            events
                .iter()
                .any(|event| event.snapshot.as_ref().is_some_and(|snapshot| {
                    snapshot.sync.provider == ProviderId::EasyPost
                        && snapshot.sync.tracker_id.is_some()
                }))
        );
        if *carrier == "USPS" {
            assert_eq!(shipment.sync.provider, ProviderId::EasyPost);
            assert!(shipment.sync.tracker_id.is_some());
        } else {
            assert_eq!(shipment.sync.provider, ProviderId::AfterShip);
            assert!(shipment.sync.tracker_id.is_none());
            assert_eq!(shipment.sync.registration_state, RegistrationState::Pending);
            assert!(shipment.sync.next_attempt_at.is_some());
        }
        let sources: i64 = sqlx::query_scalar("SELECT count(*) FROM sources WHERE shipment_id=?")
            .bind(id.to_string())
            .fetch_one(upgraded.pool())
            .await
            .unwrap();
        assert_eq!(sources, 1);
    }
    let replays: i64 = sqlx::query_scalar("SELECT count(*) FROM idempotency")
        .fetch_one(upgraded.pool())
        .await
        .unwrap();
    assert_eq!(replays, 3);
    for key in [
        "create:legacy-0",
        "create:legacy-1",
        "create:legacy-2",
        "create:orphan-doordash-replay",
    ] {
        let binding: Option<String> = sqlx::query_scalar(
            "SELECT fingerprint FROM admission_requests WHERE actor_id='owner' AND operation_key=?",
        )
        .bind(key)
        .fetch_optional(upgraded.pool())
        .await
        .unwrap();
        assert!(
            binding.is_none(),
            "Deleted DoorDash admission binding remains: {key}"
        );
    }
    for (actor, key, fingerprint) in [
        ("owner", "legacy-0", "unprefixed-operation-control"),
        ("other-owner", "create:legacy-0", "other-actor-control"),
        ("owner", "create:unrelated", "unrelated-request-control"),
    ] {
        let binding: String = sqlx::query_scalar(
            "SELECT fingerprint FROM admission_requests WHERE actor_id=? AND operation_key=?",
        )
        .bind(actor)
        .bind(key)
        .fetch_one(upgraded.pool())
        .await
        .unwrap();
        assert_eq!(binding, fingerprint);
    }
    for (index, carrier) in [(3, Carrier::Usps), (4, Carrier::SpeedX), (5, Carrier::Sway)] {
        let input = legacy_input(index, carrier);
        shipments::admission::bind_request(
            &upgraded,
            "owner",
            &format!("create:legacy-{index}"),
            &input,
        )
        .await
        .unwrap();
        let fingerprint: String = sqlx::query_scalar(
            "SELECT fingerprint FROM admission_requests WHERE actor_id='owner' AND operation_key=?",
        )
        .bind(format!("create:legacy-{index}"))
        .fetch_one(upgraded.pool())
        .await
        .unwrap();
        assert_eq!(fingerprint, admission_fingerprint(&input));
    }
    let mut corrected = legacy_input(0, Carrier::DoorDash);
    corrected.tracking.recipient_url = Some(LINK.into());
    shipments::store::validate_create(&mut corrected, "legacy-0").unwrap();
    shipments::admission::bind_request(&upgraded, "owner", "create:legacy-0", &corrected)
        .await
        .unwrap();
    let recreated = upgraded
        .create(corrected, "owner", "legacy-0", None)
        .await
        .unwrap();
    assert_ne!(recreated.id, removed[0]);
    assert_eq!(recreated.tracking.recipient_url.as_deref(), Some(LINK));
    assert!(
        sqlx::query("PRAGMA foreign_key_check")
            .fetch_all(upgraded.pool())
            .await
            .unwrap()
            .is_empty()
    );
    upgraded.close().await.unwrap();
    let reopened = Store::open(&path).await.unwrap();
    for id in removed {
        assert!(matches!(reopened.get(id).await, Err(Error::NotFound)));
    }
    for (id, carrier) in preserved {
        assert_eq!(
            reopened.get(id).await.unwrap().tracking.carrier.as_str(),
            carrier
        );
    }
    reopened.close().await.unwrap();
}

#[tokio::test]
async fn database_rejects_unlinked_doordash_inserts_and_updates_but_keeps_linked_records() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("constraints.sqlite3");
    let store = Store::open(&path).await.unwrap();
    for (index, recipient_url) in [None, Some(""), Some(" \t\n"), Some("\u{2003}")]
        .into_iter()
        .enumerate()
    {
        assert!(sqlx::query("INSERT INTO shipments(id,title,tracking_number,tracking_carrier,recipient_url) VALUES(?,'Unlinked DoorDash',?,'DoorDash',?)")
            .bind(Uuid::now_v7().to_string()).bind(format!("CONSTRAINT{index}123456")).bind(recipient_url)
            .execute(store.pool()).await.is_err());
    }
    let linked = store
        .create(
            CreateShipment {
                title: "Linked DoorDash".into(),
                direction: Direction::Inbound,
                merchant: None,
                order_reference: None,
                signature_required: None,
                source: None,
                tracking: InputTracking {
                    number: "LINKED123456".into(),
                    carrier: Some(Carrier::DoorDash),
                    recipient_url: Some(LINK.into()),
                },
            },
            "owner",
            "linked",
            None,
        )
        .await
        .unwrap();
    for recipient_url in [None, Some(""), Some(" \t\n"), Some("\u{2003}")] {
        assert!(
            sqlx::query("UPDATE shipments SET recipient_url=? WHERE id=?")
                .bind(recipient_url)
                .bind(linked.id.to_string())
                .execute(store.pool())
                .await
                .is_err()
        );
    }
    let other_id = Uuid::now_v7();
    sqlx::query("INSERT INTO shipments(id,title,tracking_number,tracking_carrier) VALUES(?,'Ordinary tracking','OTHER123456','USPS')")
        .bind(other_id.to_string()).execute(store.pool()).await.unwrap();
    assert!(
        sqlx::query("UPDATE shipments SET tracking_carrier='doordash' WHERE id=?")
            .bind(other_id.to_string())
            .execute(store.pool())
            .await
            .is_err()
    );
    assert_eq!(
        store
            .get(linked.id)
            .await
            .unwrap()
            .tracking
            .recipient_url
            .as_deref(),
        Some(LINK)
    );
    store.close().await.unwrap();
    let reopened = Store::open(&path).await.unwrap();
    assert_eq!(
        reopened
            .get(linked.id)
            .await
            .unwrap()
            .tracking
            .recipient_url
            .as_deref(),
        Some(LINK)
    );
    reopened.close().await.unwrap();
}
