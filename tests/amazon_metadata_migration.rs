//! Metadata repairs preserve owner choices and use the normal saved-page worker.

use std::{
    collections::BTreeMap,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use shipments::{
    carrier::Carrier,
    models::{
        CarrierEvent, CarrierTracking, Direction, EstimatedDeliveryWindow, EventKind,
        RegistrationState, Shipment, Source, Status, StatusOverride, SyncState, Tracking,
    },
    shipping_api::{
        PreparedRegistration, ShippingApi,
        types::{ApiFuture, ProviderId, Registration, Tracker},
    },
    store::Store,
    sync::SyncWorker,
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

#[derive(Clone, Debug, PartialEq, Eq, FromRow)]
struct DiscoveryState {
    id: i64,
    next_attempt_at: i64,
    attempts: i64,
    last_attempt_at: Option<i64>,
    lease_token: Option<String>,
    lease_until: Option<i64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct DatabaseState {
    shipments: Vec<ShipmentRow>,
    events: Vec<HistoryRow>,
    state_events: Vec<StateEventRow>,
    sources: Vec<(String, String, String)>,
    parcels: Vec<(String, String, String)>,
    discovery: DiscoveryState,
}

impl DatabaseState {
    async fn load(pool: &SqlitePool) -> Self {
        Self {
            shipments: sqlx::query_as("SELECT * FROM shipments ORDER BY id").fetch_all(pool).await.unwrap(),
            events: sqlx::query_as("SELECT * FROM events ORDER BY id").fetch_all(pool).await.unwrap(),
            state_events: sqlx::query_as("SELECT * FROM state_events ORDER BY id").fetch_all(pool).await.unwrap(),
            sources: sqlx::query_as("SELECT namespace,key,shipment_id FROM sources ORDER BY namespace,key").fetch_all(pool).await.unwrap(),
            parcels: sqlx::query_as("SELECT source_key,shipment_id,tracking_number FROM amazon_parcels ORDER BY source_key").fetch_all(pool).await.unwrap(),
            discovery: sqlx::query_as("SELECT * FROM amazon_discovery WHERE id=1").fetch_one(pool).await.unwrap(),
        }
    }

    fn unchanged_history(&self, previous: &Self, title_changes: usize) {
        assert_eq!(self.shipments.len(), previous.shipments.len());
        assert_eq!(self.sources, previous.sources);
        assert_eq!(self.parcels, previous.parcels);
        assert_eq!(self.state_events, previous.state_events);
        assert_eq!(self.discovery, previous.discovery);
        assert_eq!(
            &self.events[..previous.events.len()],
            previous.events.as_slice()
        );
        assert_eq!(self.events.len(), previous.events.len() + title_changes);
        for event in &self.events[previous.events.len()..] {
            assert_eq!(event.actor_id, "system");
            assert_eq!(event.kind, "\"updated\"");
            assert_eq!(event.detail, "Amazon Account title prefixed");
            assert!(event.snapshot.is_none());
        }
    }
}

async fn old_database(directory: &Path) -> (std::path::PathBuf, SqlitePool) {
    let migrations = directory.join("before-amazon-metadata");
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
        if version <= 22 {
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
    sqlx::query("UPDATE amazon_discovery SET next_attempt_at=5000000000,attempts=3,last_attempt_at=1234,lease_token='discovery-worker',lease_until=5000000010 WHERE id=1")
        .execute(&pool).await.unwrap();
    (database, pool)
}

struct LegacyParcel {
    shipment: Shipment,
    linked: bool,
    lease: Option<(&'static str, i64)>,
    events: Vec<(EventKind, &'static str, String)>,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "snake_case")]
enum StoredDeliveryDateKind {
    Delivered,
    Estimated,
}

#[derive(serde::Serialize)]
struct StoredDeliveryDate {
    kind: StoredDeliveryDateKind,
    date: &'static str,
    observed_at: &'static str,
    timezone: &'static str,
}

impl LegacyParcel {
    fn new(number: usize) -> Self {
        let id = Uuid::now_v7();
        let title = "Amazon: Existing parcel".to_owned();
        Self {
            shipment: Shipment {
                id,
                revision: 5,
                title: title.clone(),
                direction: Direction::Inbound,
                merchant: Some("Original merchant".into()),
                order_reference: Some("111-0000000-0000001".into()),
                signature_required: Some(true),
                tracking: Tracking {
                    carrier: Carrier::AmazonAccount,
                    number: format!("TBA{number:012}"),
                    recipient_url: None,
                },
                status: Status::Delivered,
                carrier_status: Some(Status::Delivered),
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
                    ..Default::default()
                },
                status_override: None,
                archived: false,
                created_at: "2026-10-01T09:00:00Z".into(),
                updated_at: "2026-10-02T11:00:00Z".into(),
                sources: vec![Source {
                    namespace: "fixture-mail".into(),
                    key: id.to_string(),
                }],
                sync: SyncState {
                    provider: ProviderId::AmazonAccount,
                    registration_state: RegistrationState::Registered,
                    registration_started_at: Some(123),
                    error_code: None,
                    provider_status: None,
                    provider_error_code: None,
                    tracker_id: Some(format!("synthetic-account:{id}")),
                    tracking_url: Some("https://www.amazon.com/progress-tracker/package".into()),
                    last_success_at: Some("2026-10-02T10:00:00Z".into()),
                    last_error: None,
                    next_attempt_at: None,
                    attempts: 0,
                },
            },
            linked: true,
            lease: None,
            events: vec![(EventKind::Created, "owner", title)],
        }
    }

    fn status(&mut self, status: Status) {
        self.shipment.status = status;
        self.shipment.carrier_status = Some(status);
    }

    fn override_status(&mut self, status: Status) {
        self.shipment.status = status;
        self.shipment.status_override = Some(StatusOverride {
            status,
            reason: "Owner correction".into(),
            actor_id: "owner".into(),
            set_at: "2026-10-02T11:00:00Z".into(),
        });
    }

    async fn insert(&self, pool: &SqlitePool) {
        let row = &self.shipment;
        let override_status = row.status_override.as_ref();
        let lease_token = self.lease.map(|(token, _)| token);
        let lease_until = self.lease.map(|(_, until)| until);
        sqlx::query("INSERT INTO shipments(id,revision,title,merchant,order_reference,tracking_carrier,tracking_number,tracking_generation,carrier_status,override_status,override_reason,override_actor,override_at,archived,created_at,updated_at,tracker_id,last_success_at,last_error,next_attempt_at,attempts,lease_until,lease_token,tracking_url,last_attempt_at,status_since,signature_required,carrier_tracking,direction,registration_state,registration_started_at,last_error_code,last_provider_status,last_provider_error_code,tracking_provider) VALUES(?,?,?,?,?,?,?,9,?,?,?,?,?,?,?, ?,?,?,?,?,?,?,?, ?,1234,'2026-10-01T10:00:00Z',?,?,'inbound',?,?,?,?,?,?)")
            .bind(row.id.to_string()).bind(row.revision).bind(&row.title).bind(&row.merchant).bind(&row.order_reference)
            .bind(row.tracking.carrier.as_db_key()).bind(&row.tracking.number)
            .bind(row.carrier_status.map(|status| serde_json::to_string(&status).unwrap()))
            .bind(override_status.map(|value| serde_json::to_string(&value.status).unwrap()))
            .bind(override_status.map(|value| &value.reason)).bind(override_status.map(|value| &value.actor_id)).bind(override_status.map(|value| &value.set_at))
            .bind(row.archived).bind(&row.created_at).bind(&row.updated_at).bind(&row.sync.tracker_id).bind(&row.sync.last_success_at)
            .bind(&row.sync.last_error).bind(row.sync.next_attempt_at).bind(row.sync.attempts).bind(lease_until).bind(lease_token).bind(&row.sync.tracking_url)
            .bind(row.signature_required).bind(serde_json::to_string(&row.carrier_tracking).unwrap())
            .bind(serde_json::to_string(&row.sync.registration_state).unwrap().trim_matches('"'))
            .bind(row.sync.registration_started_at).bind(&row.sync.error_code).bind(row.sync.provider_status).bind(&row.sync.provider_error_code).bind(row.sync.provider.as_str())
            .execute(pool).await.unwrap();
        for source in &row.sources {
            sqlx::query("INSERT INTO sources(namespace,key,shipment_id) VALUES(?,?,?)")
                .bind(&source.namespace)
                .bind(&source.key)
                .bind(row.id.to_string())
                .execute(pool)
                .await
                .unwrap();
        }
        if self.linked {
            sqlx::query(
                "INSERT INTO amazon_parcels(source_key,shipment_id,tracking_number) VALUES(?,?,?)",
            )
            .bind(format!("synthetic-account:{}", row.id))
            .bind(row.id.to_string())
            .bind(&row.tracking.number)
            .execute(pool)
            .await
            .unwrap();
        }
        for (kind, actor, title) in &self.events {
            let mut snapshot = row.clone();
            snapshot.title.clone_from(title);
            sqlx::query("INSERT INTO events(shipment_id,kind,actor_id,detail,snapshot) VALUES(?,?,?,'Original event',?)")
                .bind(row.id.to_string()).bind(serde_json::to_string(kind).unwrap()).bind(*actor)
                .bind(serde_json::to_string(&snapshot).unwrap()).execute(pool).await.unwrap();
        }
    }
}

#[tokio::test]
async fn generated_titles_backfill_only_when_the_initial_app_title_is_untouched() {
    let directory = tempfile::tempdir().unwrap();
    let (database, old) = old_database(directory.path()).await;
    let mut expected_titles = BTreeMap::<String, String>::new();
    for index in 0..13 {
        let mut parcel = LegacyParcel::new(index);
        parcel.shipment.title = "Generated product".into();
        parcel.events = vec![(
            EventKind::Created,
            "amazon_account",
            "Generated product".into(),
        )];
        parcel.shipment.carrier_tracking.delivered_at = Some("2026-10-02T10:00:00Z".into());
        match index {
            0 => {
                expected_titles.insert(
                    parcel.shipment.id.to_string(),
                    "Amazon: Generated product".into(),
                );
            }
            1 => {
                parcel.shipment.tracking.carrier = Carrier::Usps;
                expected_titles.insert(
                    parcel.shipment.id.to_string(),
                    "Amazon: Generated product".into(),
                );
            }
            2 => {
                parcel.shipment.title = "Owner-customized title".into();
            }
            3 => {
                parcel.events[0].1 = "owner";
            }
            4 => {
                parcel.shipment.title = "Amazon: Already prefixed".into();
                parcel.events[0].2.clone_from(&parcel.shipment.title);
            }
            5 => {
                parcel
                    .events
                    .push((EventKind::Updated, "owner", "Owner-customized title".into()));
                parcel
                    .events
                    .push((EventKind::Updated, "owner", "Generated product".into()));
            }
            6 => {
                parcel.events[0].1 = "owner";
                parcel.events.push((
                    EventKind::Created,
                    "amazon_account",
                    "Generated product".into(),
                ));
            }
            7 => {
                parcel.shipment.sync.provider = ProviderId::AfterShip;
                parcel.shipment.tracking.carrier = Carrier::AmazonMcf;
            }
            8 => {
                parcel.events.clear();
            }
            9 => {
                parcel.shipment.merchant = Some("Owner-corrected merchant".into());
                parcel
                    .events
                    .push((EventKind::Updated, "owner", "Generated product".into()));
                expected_titles.insert(
                    parcel.shipment.id.to_string(),
                    "Amazon: Generated product".into(),
                );
            }
            10..=12 => {
                parcel.shipment.title = match index {
                    10 => "x".repeat(493),
                    11 => "é".repeat(247),
                    12 => "x".repeat(492),
                    _ => unreachable!(),
                };
                parcel.events[0].2.clone_from(&parcel.shipment.title);
                if index == 12 {
                    expected_titles.insert(
                        parcel.shipment.id.to_string(),
                        format!("Amazon: {}", parcel.shipment.title),
                    );
                }
            }
            _ => unreachable!(),
        }
        parcel.insert(&old).await;
    }
    let before = DatabaseState::load(&old).await;
    old.close().await;
    let mut first_open = None;
    for _ in 0..2 {
        let store = Store::open(&database).await.unwrap();
        let after = DatabaseState::load(store.pool()).await;
        for (original, current) in before.shipments.iter().zip(&after.shipments) {
            let mut expected = original.clone();
            if let Some(title) = expected_titles.get(&current.id) {
                assert_eq!(&current.title, title);
                expected.title.clone_from(title);
                expected.revision += 1;
                assert_ne!(current.updated_at, original.updated_at);
                expected.updated_at.clone_from(&current.updated_at);
            }
            assert_eq!(current, &expected);
            store
                .get(Uuid::parse_str(&current.id).unwrap())
                .await
                .unwrap();
        }
        after.unchanged_history(&before, expected_titles.len());
        if let Some(first) = &first_open {
            assert_eq!(&after, first);
        } else {
            first_open = Some(after);
        }
        store.close().await.unwrap();
    }
}

#[tokio::test]
async fn saved_page_enrichment_preserves_ownership_and_existing_retry_decisions() {
    enum Case {
        DeliveredMissing,
        ActiveMissing,
        DeliveredKnown,
        DeliveredPromise,
        ActivePromise,
        ActiveEta,
        ActiveWindow,
        Archived,
        ManualDelivered,
        ManualCancelled,
        ManualReturned,
        Cancelled,
        Returned,
        OtherProvider,
        Unregistered,
        Unlinked,
        MissingTracker,
        Backoff,
        FutureCadence,
        Leased,
        ExpiredLease,
        ManualActive,
    }
    let directory = tempfile::tempdir().unwrap();
    let (database, old) = old_database(directory.path()).await;
    let mut due = Vec::new();
    let mut civil_sort_keys = BTreeMap::new();
    for (index, case) in [
        Case::DeliveredMissing,
        Case::ActiveMissing,
        Case::DeliveredKnown,
        Case::DeliveredPromise,
        Case::ActivePromise,
        Case::ActiveEta,
        Case::ActiveWindow,
        Case::Archived,
        Case::ManualDelivered,
        Case::ManualCancelled,
        Case::ManualReturned,
        Case::Cancelled,
        Case::Returned,
        Case::OtherProvider,
        Case::Unregistered,
        Case::Unlinked,
        Case::MissingTracker,
        Case::Backoff,
        Case::FutureCadence,
        Case::Leased,
        Case::ExpiredLease,
        Case::ManualActive,
    ]
    .into_iter()
    .enumerate()
    {
        let mut parcel = LegacyParcel::new(index);
        let has_promise = matches!(case, Case::DeliveredPromise | Case::ActivePromise);
        let active_promise = matches!(case, Case::ActivePromise);
        match case {
            Case::DeliveredMissing => {
                due.push(parcel.shipment.id.to_string());
            }
            Case::ActiveMissing => {
                parcel.status(Status::InTransit);
                due.push(parcel.shipment.id.to_string());
            }
            Case::DeliveredKnown => {
                parcel.shipment.carrier_tracking.delivered_at = Some("2026-10-02T10:00:00Z".into());
            }
            Case::DeliveredPromise => {}
            Case::ActivePromise => {
                parcel.status(Status::InTransit);
            }
            Case::ActiveEta => {
                parcel.status(Status::InTransit);
                parcel.shipment.carrier_tracking.estimated_delivery_at =
                    Some("2026-10-02T10:00:00Z".into());
            }
            Case::ActiveWindow => {
                parcel.status(Status::InTransit);
                parcel.shipment.carrier_tracking.estimated_delivery_window =
                    Some(EstimatedDeliveryWindow {
                        start: "2026-10-02T10:00:00Z".into(),
                        end: "2026-10-02T12:00:00Z".into(),
                    });
            }
            Case::Archived => {
                parcel.shipment.archived = true;
            }
            Case::ManualDelivered => {
                parcel.status(Status::InTransit);
                parcel.override_status(Status::Delivered);
            }
            Case::ManualCancelled => {
                parcel.status(Status::InTransit);
                parcel.override_status(Status::Cancelled);
            }
            Case::ManualReturned => {
                parcel.status(Status::InTransit);
                parcel.override_status(Status::ReturnToSender);
            }
            Case::Cancelled => {
                parcel.status(Status::Cancelled);
            }
            Case::Returned => {
                parcel.status(Status::ReturnToSender);
            }
            Case::OtherProvider => {
                parcel.shipment.sync.provider = ProviderId::EasyPost;
                parcel.shipment.tracking.carrier = Carrier::Usps;
            }
            Case::Unregistered => {
                parcel.shipment.sync.registration_state = RegistrationState::Pending;
            }
            Case::Unlinked => {
                parcel.linked = false;
            }
            Case::MissingTracker => {
                parcel.shipment.sync.tracker_id = None;
            }
            Case::Backoff => {
                parcel.shipment.sync.last_error = Some("Original provider failure".into());
                parcel.shipment.sync.error_code = Some("provider_http".into());
                parcel.shipment.sync.provider_status = Some(503);
                parcel.shipment.sync.provider_error_code = Some("synthetic-provider-error".into());
                parcel.shipment.sync.attempts = 4;
                parcel.shipment.sync.next_attempt_at = Some(5_000_000_000);
            }
            Case::FutureCadence => {
                parcel.status(Status::InTransit);
                parcel.shipment.sync.next_attempt_at = Some(5_000_000_000);
            }
            Case::Leased => {
                parcel.lease = Some(("current-worker", 5_000_000_000));
                parcel.shipment.sync.next_attempt_at = Some(1234);
            }
            Case::ExpiredLease => {
                parcel.lease = Some(("expired-worker", 1234));
                due.push(parcel.shipment.id.to_string());
            }
            Case::ManualActive => {
                parcel.override_status(Status::InTransit);
                due.push(parcel.shipment.id.to_string());
            }
        }
        parcel.insert(&old).await;
        if has_promise {
            // Seed the forward-compatible wire field in a real old database.
            let delivery_date = StoredDeliveryDate {
                kind: if active_promise {
                    StoredDeliveryDateKind::Estimated
                } else {
                    StoredDeliveryDateKind::Delivered
                },
                date: if active_promise {
                    "2026-11-01"
                } else {
                    "2026-10-25"
                },
                observed_at: "2026-10-31T10:00:00Z",
                timezone: "America/Los_Angeles",
            };
            sqlx::query("UPDATE shipments SET carrier_tracking=json_set(carrier_tracking,'$.delivery_date',json(?)) WHERE id=?")
                .bind(serde_json::to_string(&delivery_date).unwrap()).bind(parcel.shipment.id.to_string())
                .execute(&old).await.unwrap();
            // Migration 0024 makes existing civil metadata sortable. These are
            // ordering keys only, not fabricated delivery-scan timestamps.
            let format =
                time::format_description::parse_borrowed::<2>("[year]-[month]-[day]").unwrap();
            let key = time::Date::parse(delivery_date.date, &format)
                .unwrap()
                .midnight()
                .assume_utc()
                .unix_timestamp();
            civil_sort_keys.insert(
                parcel.shipment.id.to_string(),
                (
                    if active_promise { key } else { i64::MAX },
                    if active_promise { i64::MAX } else { -key },
                    key,
                ),
            );
        }
    }
    let earliest: i64 = sqlx::query_scalar("SELECT unixepoch()")
        .fetch_one(&old)
        .await
        .unwrap();
    let before = DatabaseState::load(&old).await;
    old.close().await;
    let mut first_open = None;
    for _ in 0..2 {
        let store = Store::open(&database).await.unwrap();
        let latest: i64 = sqlx::query_scalar("SELECT unixepoch()")
            .fetch_one(store.pool())
            .await
            .unwrap();
        let after = DatabaseState::load(store.pool()).await;
        for (original, current) in before.shipments.iter().zip(&after.shipments) {
            let mut expected = original.clone();
            if due.contains(&current.id) {
                assert!(
                    current
                        .next_attempt_at
                        .is_some_and(|at| at >= earliest && at <= latest)
                );
                expected.next_attempt_at = current.next_attempt_at;
            }
            if let Some((estimate, delivered, calendar)) = civil_sort_keys.get(&current.id) {
                expected.expected_sort_key = *estimate;
                expected.delivered_sort_key = *delivered;
                expected.calendar_sort_key = *calendar;
            }
            assert_eq!(current, &expected);
        }
        after.unchanged_history(&before, 0);
        if let Some(first) = &first_open {
            assert_eq!(&after, first);
        } else {
            first_open = Some(after);
        }
        store.close().await.unwrap();
    }
}

#[derive(Clone)]
struct SavedPageFixture {
    calls: Arc<AtomicUsize>,
    reports_date: bool,
}

impl ShippingApi for SavedPageFixture {
    fn id(&self) -> ProviderId {
        ProviderId::AmazonAccount
    }
    fn blocked(&self) -> bool {
        false
    }
    fn for_request(&self, _: &str, _: Option<&str>) -> Arc<dyn ShippingApi> {
        Arc::new(self.clone())
    }
    fn check_connection(&self) -> ApiFuture<'_, ()> {
        Box::pin(async { panic!("saved enrichment must not reconnect") })
    }
    fn reconcile_registration<'a>(&'a self, _: &'a Registration) -> ApiFuture<'a, Option<Tracker>> {
        Box::pin(async { panic!("saved enrichment must not register") })
    }
    fn prepare_registration(
        &self,
        _: Registration,
    ) -> ApiFuture<'_, Box<dyn PreparedRegistration>> {
        Box::pin(async { panic!("saved enrichment must not register") })
    }
    fn retrieve<'a>(&'a self, id: &'a str, number: &'a str) -> ApiFuture<'a, Tracker> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            Ok(Tracker {
                provider: ProviderId::AmazonAccount,
                id: id.into(),
                tracking_code: number.into(),
                carrier: Carrier::AmazonAccount,
                status: Status::Delivered,
                public_url: Some("https://www.amazon.com/progress-tracker/package".into()),
                carrier_tracking: CarrierTracking {
                    delivered_at: self.reports_date.then(|| "2026-10-02T10:00:00Z".into()),
                    ..Default::default()
                },
            })
        })
    }
}

#[tokio::test]
async fn terminal_enrichment_uses_saved_tracking_once_and_stays_stopped_after_reopen() {
    for reports_date in [true, false] {
        let directory = tempfile::tempdir().unwrap();
        let (database, old) = old_database(directory.path()).await;
        let parcel = LegacyParcel::new(1);
        parcel.insert(&old).await;
        old.close().await;
        let store = Store::open(&database).await.unwrap();
        let fixture = SavedPageFixture {
            calls: Arc::new(AtomicUsize::new(0)),
            reports_date,
        };
        let worker = SyncWorker::new(store.pool().clone(), Arc::new(fixture.clone()));
        assert!(worker.tick().await.unwrap());
        assert_eq!(fixture.calls.load(Ordering::SeqCst), 1);
        let enriched = store.get(parcel.shipment.id).await.unwrap();
        assert_eq!(enriched.id, parcel.shipment.id);
        assert_eq!(enriched.title, parcel.shipment.title);
        assert_eq!(enriched.tracking, parcel.shipment.tracking);
        assert_eq!(enriched.sources, parcel.shipment.sources);
        assert_eq!(enriched.sync.provider, parcel.shipment.sync.provider);
        assert_eq!(enriched.sync.tracker_id, parcel.shipment.sync.tracker_id);
        assert_eq!(
            enriched.sync.tracking_url,
            parcel.shipment.sync.tracking_url
        );
        assert_eq!(
            enriched.carrier_tracking.events,
            parcel.shipment.carrier_tracking.events
        );
        assert_eq!(enriched.status, Status::Delivered);
        assert_eq!(
            enriched.carrier_tracking.delivered_at.is_some(),
            reports_date
        );
        assert!(enriched.sync.next_attempt_at.is_none());
        assert!(!worker.tick().await.unwrap());
        store.close().await.unwrap();
        let reopened = Store::open(&database).await.unwrap();
        let worker = SyncWorker::new(reopened.pool().clone(), Arc::new(fixture.clone()));
        assert!(!worker.tick().await.unwrap());
        assert_eq!(fixture.calls.load(Ordering::SeqCst), 1);
        reopened.close().await.unwrap();
    }
}
