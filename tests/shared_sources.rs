use packages::{
    admission,
    error::Error,
    models::{CreateShipment, Direction, Filters, InputTracking, Source, Status},
    shipping_api::types::{ProviderId, Tracker},
    store::Store,
};

fn input(number: &str, source: Option<Source>) -> CreateShipment {
    CreateShipment {
        title: "Split order parcel".into(),
        direction: Direction::Inbound,
        merchant: Some("Example merchant".into()),
        order_reference: Some("example-order".into()),
        signature_required: None,
        tracking: InputTracking {
            carrier: Some("UPS".parse().unwrap()),
            number: number.into(),
            recipient_url: None,
        },
        source,
    }
}

fn verified(input: &CreateShipment) -> Tracker {
    Tracker {
        provider: ProviderId::EasyPost,
        id: format!("fixture_{}", input.tracking.number),
        tracking_code: input.tracking.number.clone(),
        carrier: "UPS".parse().unwrap(),
        status: Status::PreTransit,
        public_url: None,
        carrier_tracking: Default::default(),
    }
}

#[tokio::test]
async fn one_message_can_describe_multiple_parcels_without_losing_admission_fences_or_replay() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("shared-sources.sqlite3");
    let store = Store::open(&path).await.unwrap();
    let source = Source {
        namespace: "fixture-mail".into(),
        key: "split-order-message".into(),
    };
    let first = input("SPLITPARCEL001", Some(source.clone()));
    let second = input("SPLITPARCEL002", Some(source.clone()));
    let saved_first = store
        .create(first.clone(), "fixture", "first", Some(&verified(&first)))
        .await
        .unwrap();
    assert!(
        store
            .preflight_create(&second, "fixture", "second")
            .await
            .unwrap()
            .is_none(),
        "A saved parcel cannot consume the evidence for another parcel in the same email"
    );
    let claim = admission::claim_source(&store, second.source.as_ref(), "fixture", "second")
        .await
        .unwrap();
    assert!(
        admission::claim_source(&store, second.source.as_ref(), "other", "competing")
            .await
            .is_err(),
        "The existing source lease must still fence concurrent paid verification"
    );
    let saved_second = store
        .create(
            second.clone(),
            "fixture",
            "second",
            Some(&verified(&second)),
        )
        .await
        .unwrap();
    admission::release_source(&store, claim).await.unwrap();
    assert_ne!(saved_first.id, saved_second.id);
    for (request, key, expected) in [
        (&first, "first", &saved_first),
        (&second, "second", &saved_second),
    ] {
        let replay = store
            .replay_create(request, "fixture", key)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            serde_json::to_vec(&replay).unwrap(),
            serde_json::to_vec(expected).unwrap()
        );
        assert_eq!(
            store.get(expected.id).await.unwrap().sources,
            vec![source.clone()]
        );
        assert_eq!(store.events(expected.id, 0, 100).await.unwrap().len(), 1);
    }
    assert!(
        matches!(store.preflight_create(&first, "fixture", "duplicate-tracking").await,
        Err(Error::TrackingDuplicate { shipment_id }) if shipment_id == saved_first.id)
    );
    assert!(matches!(
        store.replay_create(&second, "fixture", "first").await,
        Err(Error::Conflict(_))
    ));
    let added = store
        .add_source(
            saved_second.id,
            saved_second.revision,
            source.clone(),
            "fixture",
        )
        .await
        .unwrap();
    assert_eq!(added.revision, saved_second.revision);
    let filters = Filters {
        source_namespace: Some(source.namespace.clone()),
        source_key: Some(source.key.clone()),
        ..Default::default()
    };
    let rows = store.list(&filters, None, 100).await.unwrap();
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().any(|row| row.shipment.id == saved_first.id));
    assert!(rows.iter().any(|row| row.shipment.id == saved_second.id));
    store.close().await.unwrap();
    let reopened = Store::open(&path).await.unwrap();
    assert_eq!(
        serde_json::to_vec(&reopened.get(saved_first.id).await.unwrap()).unwrap(),
        serde_json::to_vec(&saved_first).unwrap()
    );
    assert_eq!(
        serde_json::to_vec(&reopened.get(saved_second.id).await.unwrap()).unwrap(),
        serde_json::to_vec(&saved_second).unwrap()
    );
    assert_eq!(reopened.list(&filters, None, 100).await.unwrap().len(), 2);
    reopened.close().await.unwrap();
}

#[tokio::test]
async fn attaching_shared_evidence_respects_live_admission_and_is_idempotent() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(&directory.path().join("source-attachment.sqlite3"))
        .await
        .unwrap();
    let source = Source {
        namespace: "fixture-mail".into(),
        key: "shared-confirmation".into(),
    };
    let one = input("ATTACHSOURCE001", Some(source.clone()));
    let two = input("ATTACHSOURCE002", None);
    let first = store
        .create(one.clone(), "fixture", "one", Some(&verified(&one)))
        .await
        .unwrap();
    let second = store
        .create(two.clone(), "fixture", "two", Some(&verified(&two)))
        .await
        .unwrap();
    let claim = admission::claim_source(&store, Some(&source), "verifying", "third")
        .await
        .unwrap();
    assert!(matches!(
        store
            .add_source(second.id, second.revision, source.clone(), "fixture")
            .await,
        Err(Error::Conflict(_))
    ));
    assert!(store.get(second.id).await.unwrap().sources.is_empty());
    admission::release_source(&store, claim).await.unwrap();
    let added = store
        .add_source(second.id, second.revision, source.clone(), "fixture")
        .await
        .unwrap();
    assert_eq!(added.sources, first.sources);
    assert_eq!(added.revision, second.revision + 1);
    let replay = store
        .add_source(added.id, added.revision, source, "fixture")
        .await
        .unwrap();
    assert_eq!(replay.revision, added.revision);
    assert_eq!(store.events(added.id, 0, 100).await.unwrap().len(), 2);
    store.close().await.unwrap();
}

#[tokio::test]
async fn upgrading_source_links_preserves_existing_parcels_and_history() {
    use sqlx::{
        migrate::Migrator,
        sqlite::{SqliteConnectOptions, SqlitePoolOptions},
    };
    let directory = tempfile::tempdir().unwrap();
    let migrations = directory.path().join("before-shared-sources");
    std::fs::create_dir(&migrations).unwrap();
    for entry in
        std::fs::read_dir(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations"))
            .unwrap()
    {
        let entry = entry.unwrap();
        let name = entry.file_name();
        let version: u32 = name
            .to_str()
            .unwrap()
            .split_once('_')
            .unwrap()
            .0
            .parse()
            .unwrap();
        if version <= 28 {
            std::fs::copy(entry.path(), migrations.join(name)).unwrap();
        }
    }
    let path = directory.path().join("migration.sqlite3");
    let pool = SqlitePoolOptions::new()
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
        .run(&pool)
        .await
        .unwrap();
    let id = uuid::Uuid::now_v7();
    sqlx::query("INSERT INTO shipments(id,title,tracking_carrier,tracking_number) VALUES(?,?,?,?)")
        .bind(id.to_string())
        .bind("Original parcel")
        .bind("UPS")
        .bind("SPLITMIGRATION001")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO sources(namespace,key,shipment_id) VALUES('fixture-mail','migration-message',?)")
        .bind(id.to_string()).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO events(shipment_id,kind,actor_id,detail) VALUES(?,'\"created\"','fixture','Original event')")
        .bind(id.to_string()).execute(&pool).await.unwrap();
    pool.close().await;
    let store = Store::open(&path).await.unwrap();
    let original = store.get(id).await.unwrap();
    assert_eq!(original.title, "Original parcel");
    assert_eq!(original.tracking.number, "SPLITMIGRATION001");
    assert_eq!(original.revision, 1);
    assert_eq!(
        original.sources,
        vec![Source {
            namespace: "fixture-mail".into(),
            key: "migration-message".into()
        }]
    );
    assert_eq!(
        store.events(id, 0, 100).await.unwrap()[0].detail,
        "Original event"
    );
    let second = input("SPLITMIGRATION002", original.sources.first().cloned());
    let saved = store
        .create(
            second.clone(),
            "fixture",
            "new-parcel",
            Some(&verified(&second)),
        )
        .await
        .unwrap();
    assert_eq!(saved.sources, original.sources);
    assert_eq!(
        serde_json::to_vec(&store.get(id).await.unwrap()).unwrap(),
        serde_json::to_vec(&original).unwrap()
    );
    store.close().await.unwrap();
}
