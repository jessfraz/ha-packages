//! Verified account imports preserve shipment identity and user-owned state.
//! All numbers, order references and package paths here are synthetic.

use shipments::{
    amazon::AmazonParcel,
    carrier::Carrier,
    error::Error,
    models::{
        CarrierEvent, CarrierTracking, CreateShipment, Direction, Event, EventKind, InputTracking,
        Patch, SetOverride, Shipment, Source, Status, UpdateShipment,
    },
    shipping_api::types::{ProviderId, Tracker},
    store::Store,
};
use sqlx::FromRow;

struct Fixture {
    _directory: tempfile::TempDir,
    store: Store,
}

impl Fixture {
    async fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(&directory.path().join("shipments.sqlite3"))
            .await
            .unwrap();
        Self {
            _directory: directory,
            store,
        }
    }

    async fn existing(&self, number: &str, carrier: Carrier, direction: Direction) -> Shipment {
        let verified = Tracker {
            provider: ProviderId::for_carrier(Some(carrier)),
            id: "synthetic-provider-tracker".into(),
            tracking_code: number.into(),
            carrier,
            status: Status::InTransit,
            public_url: None,
            carrier_tracking: CarrierTracking {
                events: vec![scan("2026-10-01T10:00:00Z", "Existing carrier scan")],
                ..Default::default()
            },
        };
        self.store
            .create(
                CreateShipment {
                    title: "User's original parcel title".into(),
                    direction,
                    merchant: Some("Original merchant".into()),
                    order_reference: Some("original-order-reference".into()),
                    signature_required: Some(true),
                    tracking: InputTracking {
                        number: number.into(),
                        carrier: Some(carrier),
                        recipient_url: None,
                    },
                    source: Some(Source {
                        namespace: "fixture-mail".into(),
                        key: number.into(),
                    }),
                },
                "owner",
                number,
                Some(&verified),
            )
            .await
            .unwrap()
    }

    async fn counts(&self) -> (i64, i64, i64) {
        sqlx::query_as("SELECT (SELECT count(*) FROM shipments), (SELECT count(*) FROM amazon_parcels), (SELECT count(*) FROM events)")
            .fetch_one(self.store.pool()).await.unwrap()
    }
}

fn scan(at: &str, message: &str) -> CarrierEvent {
    CarrierEvent {
        status: Some(Status::InTransit),
        status_detail: None,
        datetime: Some(at.into()),
        message: Some(message.into()),
        description: None,
        source: None,
        carrier_code: None,
        tracking_location: None,
    }
}

fn parcel(package: &str, number: &str, carrier: Carrier) -> AmazonParcel {
    let path = format!(
        "/progress-tracker/package?orderId=111-0000000-0000001&shipmentId=synthetic-shipment&packageId={package}"
    );
    AmazonParcel {
        title: format!("Verified product in package {package}"),
        order_reference: "111-0000000-0000001".into(),
        source_key: format!("synthetic-account:111-0000000-0000001:synthetic-shipment:{package}"),
        tracker: Tracker {
            provider: ProviderId::AmazonAccount,
            id: path.clone(),
            tracking_code: number.into(),
            carrier,
            status: Status::OutForDelivery,
            public_url: Some(format!("https://www.amazon.com{path}")),
            carrier_tracking: CarrierTracking {
                events: vec![scan("2026-10-02T10:00:00Z", "Verified Amazon scan")],
                ..Default::default()
            },
        },
    }
}

async fn history(store: &Store, shipment: &Shipment) -> Vec<Event> {
    store.events(shipment.id, 0, 100).await.unwrap()
}

fn assert_history_prefix(before: &[Event], after: &[Event]) {
    assert!(after.len() >= before.len());
    assert_eq!(
        serde_json::to_string(before).unwrap(),
        serde_json::to_string(&after[..before.len()]).unwrap(),
        "Import must preserve existing event contents, not only their IDs"
    );
}

#[tokio::test]
async fn repeated_import_and_reopened_store_reuse_one_parcel_without_extra_history() {
    let f = Fixture::new().await;
    let p = parcel("1", "TBA000000000001", Carrier::AmazonMcf);
    let first = f.store.import_amazon(&p, None).await.unwrap();
    let events = history(&f.store, &first).await;
    assert_eq!(first.title, p.title);
    assert_eq!(first.direction, Direction::Inbound);
    assert_eq!(first.sync.provider, ProviderId::AmazonAccount);
    assert_eq!(first.tracking.carrier, Carrier::AmazonMcf);
    assert_eq!(
        first.sources,
        vec![Source {
            namespace: "amazon-account".into(),
            key: p.source_key.clone()
        }]
    );
    let second = f.store.import_amazon(&p, None).await.unwrap();
    assert_eq!((second.id, second.revision), (first.id, first.revision));
    assert_eq!(f.counts().await, (1, 1, 1));
    f.store.close().await.unwrap();
    let reopened = Store::open(&f._directory.path().join("shipments.sqlite3"))
        .await
        .unwrap();
    let third = reopened.import_amazon(&p, None).await.unwrap();
    assert_eq!((third.id, third.revision), (first.id, first.revision));
    assert_history_prefix(&events, &history(&reopened, &third).await);
    assert_eq!(history(&reopened, &third).await.len(), events.len());
}

#[tokio::test]
async fn usps_number_deduplicates_preserving_user_fields_provider_scans_and_override() {
    let f = Fixture::new().await;
    let existing = f
        .existing("9400000000000000000001", Carrier::Usps, Direction::Inbound)
        .await;
    let before = f
        .store
        .set_override(
            existing.id,
            existing.revision,
            Some(SetOverride {
                status: Status::Delivered,
                reason: "Owner confirmed delivery".into(),
            }),
            "owner",
        )
        .await
        .unwrap();
    let events = history(&f.store, &before).await;
    let p = parcel("1", &before.tracking.number, Carrier::Usps);
    let linked = f.store.import_amazon(&p, None).await.unwrap();
    assert_eq!(linked.id, before.id);
    assert_eq!(linked.title, before.title);
    assert_eq!(linked.merchant, before.merchant);
    assert_eq!(linked.order_reference, before.order_reference);
    assert_eq!(linked.signature_required, before.signature_required);
    assert_eq!(linked.tracking, before.tracking);
    assert_eq!(linked.sync.provider, ProviderId::EasyPost);
    assert_eq!(linked.sync.tracker_id, before.sync.tracker_id);
    assert_eq!(linked.status, Status::Delivered);
    assert_eq!(
        serde_json::to_string(&linked.status_override).unwrap(),
        serde_json::to_string(&before.status_override).unwrap()
    );
    assert_eq!(linked.carrier_tracking, before.carrier_tracking);
    assert!(linked.sources.contains(&before.sources[0]));
    assert!(
        linked
            .sources
            .iter()
            .any(|source| source.namespace == "amazon-account" && source.key == p.source_key)
    );
    let after = history(&f.store, &linked).await;
    assert_history_prefix(&events, &after);
    assert_eq!(after.len(), events.len() + 1);
    assert_eq!(f.counts().await.0, 1);
}

#[tokio::test]
async fn confirmed_amazon_fleet_adoption_merges_history_and_retains_manual_override() {
    let f = Fixture::new().await;
    let original = f
        .existing("TBA000000000002", Carrier::AmazonMcf, Direction::Inbound)
        .await;
    let before = f
        .store
        .set_override(
            original.id,
            original.revision,
            Some(SetOverride {
                status: Status::Delivered,
                reason: "Owner confirmed delivery".into(),
            }),
            "owner",
        )
        .await
        .unwrap();
    let events = history(&f.store, &before).await;
    let p = parcel("2", &before.tracking.number, Carrier::AmazonMcf);
    let linked = f.store.import_amazon(&p, None).await.unwrap();
    assert_eq!(linked.id, before.id);
    assert_eq!(linked.title, before.title);
    assert_eq!(linked.sync.provider, ProviderId::AmazonAccount);
    assert_eq!(
        linked.sync.tracker_id.as_deref(),
        Some(p.tracker.id.as_str())
    );
    assert_eq!(linked.status, Status::Delivered);
    assert!(
        linked
            .carrier_tracking
            .events
            .contains(&before.carrier_tracking.events[0])
    );
    assert!(
        linked
            .carrier_tracking
            .events
            .contains(&p.tracker.carrier_tracking.events[0])
    );
    assert_history_prefix(&events, &history(&f.store, &linked).await);
    let (generation, next): (i64, Option<i64>) =
        sqlx::query_as("SELECT tracking_generation,next_attempt_at FROM shipments WHERE id=?")
            .bind(linked.id.to_string())
            .fetch_one(f.store.pool())
            .await
            .unwrap();
    assert_eq!(generation, 1);
    assert_eq!(next, None);
}

#[tokio::test]
async fn split_packages_share_order_but_keep_distinct_items_numbers_and_sources() {
    let f = Fixture::new().await;
    let a = parcel("1", "TBA000000000003", Carrier::AmazonMcf);
    let b = parcel("2", "9400000000000000000002", Carrier::Usps);
    let first = f.store.import_amazon(&a, None).await.unwrap();
    let second = f.store.import_amazon(&b, None).await.unwrap();
    assert_ne!(first.id, second.id);
    assert_eq!(first.order_reference, second.order_reference);
    assert_eq!(first.title, a.title);
    assert_eq!(second.title, b.title);
    assert_ne!(first.tracking.number, second.tracking.number);
    assert_ne!(first.sources, second.sources);
    assert_eq!(f.counts().await, (2, 2, 2));
}

#[tokio::test]
async fn changed_number_for_same_package_is_deferred_without_creating_another_record() {
    let f = Fixture::new().await;
    let mut p = parcel("1", "TBA000000000004", Carrier::AmazonMcf);
    let first = f.store.import_amazon(&p, None).await.unwrap();
    p.tracker.tracking_code = "9400000000000000000003".into();
    p.tracker.carrier = Carrier::Usps;
    assert!(matches!(
        f.store.import_amazon(&p, None).await,
        Err(Error::Conflict(_))
    ));
    assert_eq!(f.counts().await, (1, 1, 1));
    assert_eq!(
        f.store.get(first.id).await.unwrap().tracking,
        first.tracking
    );
}

#[tokio::test]
async fn archived_matching_record_stays_archived_and_keeps_its_tracking_provider() {
    let f = Fixture::new().await;
    let original = f
        .existing("TBA000000000005", Carrier::AmazonMcf, Direction::Inbound)
        .await;
    let archived = f
        .store
        .archive(original.id, original.revision, true, "owner")
        .await
        .unwrap();
    let p = parcel("1", &archived.tracking.number, Carrier::AmazonMcf);
    let linked = f.store.import_amazon(&p, None).await.unwrap();
    assert_eq!(linked.id, archived.id);
    assert!(linked.archived);
    assert_eq!(linked.sync.provider, ProviderId::AfterShip);
    assert_eq!(linked.carrier_tracking, archived.carrier_tracking);
    let scheduled: Option<i64> =
        sqlx::query_scalar("SELECT next_attempt_at FROM shipments WHERE id=?")
            .bind(linked.id.to_string())
            .fetch_one(f.store.pool())
            .await
            .unwrap();
    assert_eq!(scheduled, None);
    assert_eq!(f.counts().await.0, 1);
}

#[tokio::test]
async fn conflicting_existing_direction_or_carrier_is_not_reinterpreted_as_amazon() {
    for (carrier, direction) in [
        (Carrier::Usps, Direction::Outbound),
        (Carrier::Ups, Direction::Inbound),
    ] {
        let f = Fixture::new().await;
        let before = f
            .existing("9400000000000000000004", carrier, direction)
            .await;
        let p = parcel("1", &before.tracking.number, Carrier::Usps);
        assert!(matches!(
            f.store.import_amazon(&p, None).await,
            Err(Error::Conflict(_))
        ));
        let after = f.store.get(before.id).await.unwrap();
        assert_eq!(
            serde_json::to_string(&after).unwrap(),
            serde_json::to_string(&before).unwrap()
        );
        assert_eq!(f.counts().await, (1, 0, 1));
    }
}

#[tokio::test]
async fn public_source_provenance_alone_never_selects_the_amazon_account_provider() {
    let f = Fixture::new().await;
    let original = f
        .existing("TBA000000000006", Carrier::AmazonMcf, Direction::Inbound)
        .await;
    let p = parcel("1", &original.tracking.number, Carrier::AmazonMcf);
    let changed = f
        .store
        .add_source(
            original.id,
            original.revision,
            Source {
                namespace: "amazon-account".into(),
                key: p.source_key,
            },
            "agent:fixture",
        )
        .await
        .unwrap();
    assert_eq!(changed.sync.provider, ProviderId::AfterShip);
    assert_eq!(f.counts().await.1, 0);
    let edited = f
        .store
        .update(
            changed.id,
            changed.revision,
            UpdateShipment {
                title: Some("Edited by an agent".into()),
                ..Default::default()
            },
            "agent:fixture",
            None,
        )
        .await
        .unwrap();
    assert_eq!(edited.sync.provider, ProviderId::AfterShip);
    assert_eq!(f.counts().await.1, 0);
}

#[tokio::test]
async fn metadata_edits_and_unchanged_tracking_resubmissions_preserve_amazon_provider() {
    let f = Fixture::new().await;
    let p = parcel("1", "TBA000000000007", Carrier::AmazonMcf);
    let mut before = f.store.import_amazon(&p, None).await.unwrap();
    for resubmit in [false, true] {
        let edited = f
            .store
            .update(
                before.id,
                before.revision,
                UpdateShipment {
                    title: Some("Owner's corrected product title".into()),
                    tracking: if resubmit {
                        Patch {
                            value: Some(Some(before.tracking.clone().into())),
                        }
                    } else {
                        Patch::default()
                    },
                    ..Default::default()
                },
                "owner",
                None,
            )
            .await
            .unwrap();
        assert_eq!(edited.sync.provider, ProviderId::AmazonAccount);
        assert_eq!(edited.sync.tracker_id, before.sync.tracker_id);
        assert_eq!(edited.tracking, before.tracking);
        assert_eq!(edited.carrier_tracking, before.carrier_tracking);
        before = edited;
    }
}

#[tokio::test]
async fn old_discovery_observation_cannot_recreate_tracking_replaced_by_the_owner() {
    let f = Fixture::new().await;
    let p = parcel("1", "TBA000000000008", Carrier::AmazonMcf);
    let original = f.store.import_amazon(&p, None).await.unwrap();
    let edited = f
        .store
        .update(
            original.id,
            original.revision,
            UpdateShipment {
                tracking: Patch {
                    value: Some(Some(InputTracking {
                        number: "TBA000000000009".into(),
                        carrier: Some(Carrier::AmazonMcf),
                        recipient_url: None,
                    })),
                },
                ..Default::default()
            },
            "owner",
            None,
        )
        .await
        .unwrap();
    assert!(matches!(
        f.store.import_amazon(&p, None).await,
        Err(Error::Conflict(_))
    ));
    assert_eq!(f.counts().await.0, 1);
    assert_eq!(
        f.store.get(edited.id).await.unwrap().tracking.number,
        "TBA000000000009"
    );
}

#[tokio::test]
async fn discovery_lease_fences_creation_and_import_replays() {
    let f = Fixture::new().await;
    let p = parcel("1", "TBA000000000010", Carrier::AmazonMcf);
    sqlx::query(
        "UPDATE amazon_discovery SET lease_token='stale',lease_until=unixepoch()-1 WHERE id=1",
    )
    .execute(f.store.pool())
    .await
    .unwrap();
    assert!(matches!(
        f.store.import_amazon(&p, Some("stale")).await,
        Err(Error::Conflict(_))
    ));
    assert_eq!(f.counts().await, (0, 0, 0));
    sqlx::query(
        "UPDATE amazon_discovery SET lease_token='current',lease_until=unixepoch()+60 WHERE id=1",
    )
    .execute(f.store.pool())
    .await
    .unwrap();
    let imported = f.store.import_amazon(&p, Some("current")).await.unwrap();
    assert!(matches!(
        f.store.import_amazon(&p, Some("stale")).await,
        Err(Error::Conflict(_))
    ));
    assert_eq!(f.counts().await, (1, 1, 1));
    assert_eq!(f.store.get(imported.id).await.unwrap().title, p.title);
}

#[tokio::test]
async fn concurrent_duplicate_imports_commit_one_record_and_one_created_event() {
    let f = Fixture::new().await;
    let p = parcel("1", "TBA000000000011", Carrier::AmazonMcf);
    let (first, second) = tokio::join!(
        f.store.import_amazon(&p, None),
        f.store.import_amazon(&p, None)
    );
    let first = first.unwrap();
    assert_eq!(first.id, second.unwrap().id);
    assert_eq!(f.counts().await, (1, 1, 1));
    assert!(matches!(
        history(&f.store, &first).await[0].kind,
        EventKind::Created
    ));
}

#[derive(FromRow)]
struct QueryPlan {
    detail: String,
}

#[tokio::test]
async fn import_identity_lookups_use_indexes_with_representative_saved_parcels() {
    let f = Fixture::new().await;
    let mut target = None;
    for index in 1..=100 {
        let p = parcel(
            &index.to_string(),
            &format!("TBA{index:012}"),
            Carrier::AmazonMcf,
        );
        target = Some((
            p.source_key.clone(),
            f.store.import_amazon(&p, None).await.unwrap(),
        ));
    }
    let (source_key, target) = target.unwrap();
    let tracking: Vec<QueryPlan> =
        sqlx::query_as("EXPLAIN QUERY PLAN SELECT id FROM shipments WHERE tracking_number=?")
            .bind(&target.tracking.number)
            .fetch_all(f.store.pool())
            .await
            .unwrap();
    let mapping: Vec<QueryPlan> = sqlx::query_as("EXPLAIN QUERY PLAN SELECT shipment_id,tracking_number FROM amazon_parcels WHERE source_key=?")
        .bind(&source_key).fetch_all(f.store.pool()).await.unwrap();
    for row in &tracking {
        println!("Tracking identity plan: {}", row.detail);
    }
    for row in &mapping {
        println!("Amazon parcel identity plan: {}", row.detail);
    }
    assert!(
        tracking
            .iter()
            .any(|row| row.detail.contains("SEARCH shipments")
                && row.detail.contains("shipments_tracking_number"))
    );
    assert!(
        mapping
            .iter()
            .any(|row| row.detail.contains("SEARCH amazon_parcels")
                && row.detail.contains("source_key=?"))
    );
    assert!(
        tracking
            .iter()
            .chain(&mapping)
            .all(|row| !row.detail.starts_with("SCAN "))
    );
    let found: String = sqlx::query_scalar("SELECT id FROM shipments WHERE tracking_number=?")
        .bind(&target.tracking.number)
        .fetch_one(f.store.pool())
        .await
        .unwrap();
    assert_eq!(found, target.id.to_string());
}

#[tokio::test]
async fn unverified_or_oversized_observations_leave_no_partial_parcels_or_history() {
    for condition in ["provider", "status", "title", "source"] {
        let f = Fixture::new().await;
        let mut p = parcel("1", "TBA000000000012", Carrier::AmazonMcf);
        match condition {
            "provider" => p.tracker.provider = ProviderId::EasyPost,
            "status" => p.tracker.status = Status::Unknown,
            "title" => p.title = "X".repeat(501),
            "source" => p.source_key = "X".repeat(501),
            _ => unreachable!(),
        }
        assert!(
            matches!(
                f.store.import_amazon(&p, None).await,
                Err(Error::Invalid(_))
            ),
            "Invalid {condition} was saved"
        );
        assert_eq!(f.counts().await, (0, 0, 0));
    }
}

#[tokio::test]
async fn imported_numbers_obey_the_existing_shipment_admission_contract() {
    for number in [
        "A1".to_owned(),
        "PLACEHOLDER".to_owned(),
        "A".repeat(100) + "1",
        "SYNTHETIC_0001".to_owned(),
    ] {
        let f = Fixture::new().await;
        let p = parcel("1", &number, Carrier::AmazonMcf);
        assert!(
            matches!(
                f.store.import_amazon(&p, None).await,
                Err(Error::TrackingInvalid)
            ),
            "An invalid or placeholder tracking number was imported"
        );
        assert_eq!(f.counts().await, (0, 0, 0));
    }
}

#[derive(Debug, FromRow, PartialEq, Eq)]
struct ScheduledWork {
    tracking_generation: i64,
    next_attempt_at: Option<i64>,
    lease_token: Option<String>,
    lease_until: Option<i64>,
    attempts: i64,
    last_error: Option<String>,
    last_error_code: Option<String>,
    last_provider_status: Option<i64>,
    last_provider_error_code: Option<String>,
    registration_state: String,
}

async fn scheduled_work(store: &Store, shipment: &Shipment) -> ScheduledWork {
    sqlx::query_as(
        "SELECT tracking_generation,next_attempt_at,lease_token,lease_until,attempts,
                last_error,last_error_code,last_provider_status,last_provider_error_code,
                registration_state FROM shipments WHERE id=?",
    )
    .bind(shipment.id.to_string())
    .fetch_one(store.pool())
    .await
    .unwrap()
}

async fn scoped_import(
    store: &Store,
    scope: &str,
    package: &str,
    number: &str,
    carrier: Carrier,
    status: Status,
) -> Shipment {
    let mut p = parcel(package, number, carrier);
    p.source_key = format!("{scope}{}", p.source_key);
    p.tracker.id = format!("{scope}{}", p.tracker.id);
    p.tracker.status = status;
    store.import_amazon(&p, None).await.unwrap()
}

#[tokio::test]
async fn pausing_amazon_fences_both_accounts_and_discovery_without_touching_easypost() {
    let f = Fixture::new().await;
    let current = format!("{}:", "a".repeat(64));
    let other = format!("{}:", "b".repeat(64));
    let first = scoped_import(
        &f.store,
        &current,
        "1",
        "TBA000000000021",
        Carrier::AmazonMcf,
        Status::InTransit,
    )
    .await;
    let second = scoped_import(
        &f.store,
        &other,
        "2",
        "9400000000000000000021",
        Carrier::Usps,
        Status::InTransit,
    )
    .await;
    let postal = f
        .existing("9400000000000000000022", Carrier::Usps, Direction::Inbound)
        .await;
    // Represent requests already in flight for both account generations and a
    // different provider. Disconnect must revoke only Amazon's ownership.
    sqlx::query("UPDATE shipments SET tracking_generation=7,lease_token='in-flight',lease_until=unixepoch()+60,next_attempt_at=unixepoch()+300")
        .execute(f.store.pool()).await.unwrap();
    sqlx::query("UPDATE amazon_discovery SET lease_token='discovery-in-flight',lease_until=unixepoch()+60 WHERE id=1")
        .execute(f.store.pool()).await.unwrap();
    let postal_before = scheduled_work(&f.store, &postal).await;
    let first_history = history(&f.store, &first).await;
    let second_history = history(&f.store, &second).await;
    f.store.pause_amazon().await.unwrap();
    for shipment in [&first, &second] {
        let work = scheduled_work(&f.store, shipment).await;
        assert_eq!(work.tracking_generation, 8);
        assert_eq!(work.next_attempt_at, None);
        assert_eq!(work.lease_token, None);
        assert_eq!(work.lease_until, None);
        let saved = f.store.get(shipment.id).await.unwrap();
        assert_eq!(saved.tracking, shipment.tracking);
        assert_eq!(saved.title, shipment.title);
    }
    assert_eq!(scheduled_work(&f.store, &postal).await, postal_before);
    let discovery: (Option<String>, Option<i64>) =
        sqlx::query_as("SELECT lease_token,lease_until FROM amazon_discovery WHERE id=1")
            .fetch_one(f.store.pool())
            .await
            .unwrap();
    assert_eq!(discovery, (None, None));
    assert_history_prefix(&first_history, &history(&f.store, &first).await);
    assert_eq!(history(&f.store, &first).await.len(), first_history.len());
    assert_history_prefix(&second_history, &history(&f.store, &second).await);
    assert_eq!(history(&f.store, &second).await.len(), second_history.len());
}

#[tokio::test]
async fn reconnect_resumes_only_current_effectively_active_parcels_and_rejects_bad_scopes() {
    let f = Fixture::new().await;
    let current = format!("{}:", "a".repeat(64));
    let other = format!("{}:", "b".repeat(64));
    let mut parcels = Vec::new();
    for (package, status, scope) in [
        ("1", Status::InTransit, &current),
        ("2", Status::Delivered, &current),
        ("3", Status::InTransit, &current),
        ("4", Status::ReturnToSender, &current),
        ("5", Status::Cancelled, &current),
        ("6", Status::InTransit, &current),
        ("7", Status::InTransit, &other),
        ("8", Status::Delivered, &current),
    ] {
        parcels.push(
            scoped_import(
                &f.store,
                scope,
                package,
                &format!("TBA{package:0>12}"),
                Carrier::AmazonMcf,
                status,
            )
            .await,
        );
    }
    let override_terminal = f
        .store
        .set_override(
            parcels[2].id,
            parcels[2].revision,
            Some(SetOverride {
                status: Status::Delivered,
                reason: "Owner confirmed delivery".into(),
            }),
            "owner",
        )
        .await
        .unwrap();
    let archived = f
        .store
        .archive(parcels[5].id, parcels[5].revision, true, "owner")
        .await
        .unwrap();
    // The owner's nonterminal correction is the effective state, even when the
    // carrier last reported a terminal status; coalesce(override,carrier) wins.
    let corrected_active = f
        .store
        .set_override(
            parcels[7].id,
            parcels[7].revision,
            Some(SetOverride {
                status: Status::InTransit,
                reason: "Owner corrected carrier status".into(),
            }),
            "owner",
        )
        .await
        .unwrap();
    let postal = f
        .existing("9400000000000000000030", Carrier::Usps, Direction::Inbound)
        .await;
    sqlx::query("UPDATE shipments SET attempts=4,last_error='Sign-in required',last_error_code='provider_authentication_required',last_provider_status=401,last_provider_error_code='INVALID_GRANT',registration_state='rejected' WHERE tracking_provider='amazon_account'")
        .execute(f.store.pool()).await.unwrap();
    let postal_before = scheduled_work(&f.store, &postal).await;
    f.store.pause_amazon().await.unwrap();
    let mut paused = Vec::new();
    for shipment in &parcels {
        paused.push(scheduled_work(&f.store, shipment).await);
    }
    // Include a 65-byte non-ASCII scope: validating its prefix must not slice a
    // UTF-8 string at an invalid boundary or panic.
    for invalid in [
        String::new(),
        format!("{}:", "a".repeat(63)),
        format!("{}:", "a".repeat(65)),
        format!("{}:", "g".repeat(64)),
        "a".repeat(65),
        format!("{}:", "é".repeat(32)),
    ] {
        assert!(matches!(
            f.store.resume_amazon(&invalid).await,
            Err(Error::Invalid(_))
        ));
        assert_eq!(scheduled_work(&f.store, &parcels[0]).await, paused[0]);
    }
    let before: i64 = sqlx::query_scalar("SELECT unixepoch()")
        .fetch_one(f.store.pool())
        .await
        .unwrap();
    f.store.resume_amazon(&current).await.unwrap();
    let after: i64 = sqlx::query_scalar("SELECT unixepoch()")
        .fetch_one(f.store.pool())
        .await
        .unwrap();
    for (index, shipment) in parcels.iter().enumerate() {
        let work = scheduled_work(&f.store, shipment).await;
        if [0, 7].contains(&index) {
            assert_eq!(
                work.tracking_generation,
                paused[index].tracking_generation + 1
            );
            assert!(
                work.next_attempt_at
                    .is_some_and(|due| (before..=after).contains(&due))
            );
            assert_eq!(work.lease_token, None);
            assert_eq!(work.lease_until, None);
            assert_eq!(work.attempts, 0);
            assert_eq!(work.last_error, None);
            assert_eq!(work.last_error_code, None);
            assert_eq!(work.last_provider_status, None);
            assert_eq!(work.last_provider_error_code, None);
            assert_eq!(work.registration_state, "registered");
        } else {
            assert_eq!(
                work, paused[index],
                "Terminal, archived or other-account parcel resumed"
            );
        }
    }
    assert_eq!(scheduled_work(&f.store, &postal).await, postal_before);
    assert_eq!(
        f.store.get(override_terminal.id).await.unwrap().status,
        Status::Delivered
    );
    assert!(f.store.get(archived.id).await.unwrap().archived);
    assert_eq!(
        f.store.get(corrected_active.id).await.unwrap().status,
        Status::InTransit
    );
}
