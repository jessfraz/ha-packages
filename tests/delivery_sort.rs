use std::sync::Arc;

use dropshot::{HttpServer, ResultsPage};
use reqwest::{Client, StatusCode};
use shipments::{
    api,
    carrier::Carrier,
    models::{
        CarrierEvent, CarrierTracking, CreateShipment, InputTracking, SetOverride, Shipment,
        Source, Status,
    },
    shipping_api::types::Tracker,
    store::Store,
};
use sqlx::SqlitePool;
use uuid::Uuid;

const OWNER: &str = "delivery-sort-owner-credential-at-least-32-bytes";

struct App {
    _directory: tempfile::TempDir,
    store: Store,
    pool: SqlitePool,
    server: HttpServer<Arc<api::Context>>,
    client: Client,
    url: String,
}

impl App {
    async fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("delivery-sort.sqlite3");
        let store = Store::open(&path).await.unwrap();
        let pool = SqlitePool::connect(&format!("sqlite://{}", path.display()))
            .await
            .unwrap();
        let server = api::start(
            "127.0.0.1:0".parse().unwrap(),
            Arc::new(api::Context::new(store.clone(), OWNER, None, None)),
        )
        .unwrap();
        let url = format!("http://{}", server.local_addr());
        Self {
            _directory: directory,
            store,
            pool,
            server,
            client: Client::new(),
            url,
        }
    }

    async fn create(
        &self,
        key: &str,
        status: Status,
        expected: Option<&str>,
        delivered: Option<&str>,
    ) -> Shipment {
        let tracking = InputTracking {
            carrier: Some("usps".parse().unwrap()),
            number: key.to_ascii_uppercase(),
        };
        let tracker = Tracker {
            id: format!("trk_{key}"),
            tracking_code: tracking.number.clone(),
            carrier: "usps".parse().unwrap(),
            status,
            public_url: None,
            carrier_tracking: CarrierTracking {
                estimated_delivery_at: expected.map(str::to_owned),
                delivered_at: delivered.map(str::to_owned),
                events: delivered
                    .map(|datetime| CarrierEvent {
                        status: Some(Status::Delivered),
                        datetime: Some(datetime.into()),
                        ..Default::default()
                    })
                    .into_iter()
                    .collect(),
                ..Default::default()
            },
        };
        self.store
            .create(
                CreateShipment {
                    direction: shipments::models::Direction::Inbound,
                    title: format!("Date parcel {key}"),
                    merchant: None,
                    order_reference: None,
                    signature_required: None,
                    tracking,
                    source: Some(Source {
                        namespace: "sort-test".into(),
                        key: key.into(),
                    }),
                },
                "test",
                key,
                Some(&tracker),
            )
            .await
            .unwrap()
    }

    async fn page(&self, query: &[(&str, &str)]) -> ResultsPage<Shipment> {
        let response = self
            .client
            .get(format!("{}/v1/shipments", self.url))
            .bearer_auth(OWNER)
            .query(query)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        response.json().await.unwrap()
    }

    async fn all(&self, query: &[(&str, &str)]) -> Vec<Shipment> {
        let mut items = Vec::new();
        let mut page = self.page(query).await;
        loop {
            items.extend(page.items);
            let Some(token) = page.next_page else {
                return items;
            };
            page = self.page(&[("limit", "2"), ("page_token", &token)]).await;
        }
    }

    async fn close(self) {
        self.server.close().await.unwrap();
        self.pool.close().await;
        self.store.close().await.unwrap();
    }
}

fn ids(shipments: &[Shipment]) -> Vec<Uuid> {
    shipments.iter().map(|shipment| shipment.id).collect()
}

#[tokio::test]
async fn expected_sort_is_global_stable_and_keeps_unknown_dates_last() {
    let app = App::new().await;
    let unknown = app.create("unknown", Status::InTransit, None, None).await;
    let late = app
        .create("late", Status::Failure, Some("2026-10-21T12:00:00Z"), None)
        .await;
    let first = app
        .create(
            "first",
            Status::InTransit,
            Some("2026-10-16T12:00:00Z"),
            None,
        )
        .await;
    let tied = app
        .create(
            "tied",
            Status::InTransit,
            Some("2026-10-16T07:00:00-05:00"),
            None,
        )
        .await;
    let middle = app
        .create("middle", Status::Error, Some("2026-10-18T12:00:00Z"), None)
        .await;
    let unknown_tied = app
        .create("unknown-tied", Status::InTransit, None, None)
        .await;
    let hidden = app
        .create(
            "hidden",
            Status::InTransit,
            Some("2026-10-01T00:00:00Z"),
            None,
        )
        .await;
    app.store
        .archive(hidden.id, hidden.revision, true, "test")
        .await
        .unwrap();
    let expected = vec![
        first.id,
        tied.id,
        middle.id,
        late.id,
        unknown.id,
        unknown_tied.id,
    ];
    let records = app
        .all(&[
            ("sort", "expected"),
            ("active", "true"),
            ("search", "Date parcel"),
            ("carrier", "USPS"),
            ("limit", "2"),
        ])
        .await;
    assert_eq!(ids(&records), expected);
    assert_eq!(records[0].sources[0].key, "first");
    let exceptions = app
        .all(&[
            ("sort", "expected"),
            ("exceptions", "true"),
            ("search", "Date parcel"),
            ("limit", "1"),
        ])
        .await;
    assert_eq!(ids(&exceptions), vec![middle.id, late.id]);
    let archived = app
        .all(&[("sort", "expected"), ("archived", "true"), ("limit", "2")])
        .await;
    assert_eq!(ids(&archived), vec![hidden.id]);
    let sourced = app
        .all(&[
            ("sort", "expected"),
            ("source_namespace", "sort-test"),
            ("source_key", "middle"),
        ])
        .await;
    assert_eq!(ids(&sourced), vec![middle.id]);

    let page = app
        .page(&[("sort", "expected"), ("active", "true"), ("limit", "2")])
        .await;
    assert_eq!(ids(&page.items), expected[..2]);
    let cursor = page.next_page.unwrap();
    // Change the last selected row's date. The cursor must retain the original
    // date instead of querying the current cursor row and repeating earlier rows.
    sqlx::query("UPDATE shipments SET carrier_tracking=json_set(carrier_tracking,'$.estimated_delivery_at',?) WHERE id=?")
        .bind("2026-10-01T00:00:00Z").bind(tied.id.to_string()).execute(&app.pool).await.unwrap();
    let remaining = app.all(&[("page_token", &cursor), ("limit", "2")]).await;
    assert_eq!(ids(&remaining), expected[2..]);
    app.store
        .archive(tied.id, tied.revision, true, "test")
        .await
        .unwrap();
    let remaining = app.all(&[("page_token", &cursor), ("limit", "2")]).await;
    assert_eq!(ids(&remaining), expected[2..]);
    app.close().await;
}

#[tokio::test]
async fn delivered_sort_uses_actual_dates_manual_corrections_and_stable_unknowns() {
    let app = App::new().await;
    let unknown = app
        .create(
            "unknown",
            Status::Delivered,
            Some("2099-01-01T00:00:00Z"),
            None,
        )
        .await;
    let old = app
        .create(
            "old",
            Status::Delivered,
            Some("2098-01-01T00:00:00Z"),
            Some("2026-10-10T00:00:00Z"),
        )
        .await;
    let recent = app
        .create(
            "recent",
            Status::Delivered,
            None,
            Some("2026-10-15T12:00:00Z"),
        )
        .await;
    let tied = app
        .create(
            "tied",
            Status::Delivered,
            None,
            Some("2026-10-15T05:00:00-07:00"),
        )
        .await;
    let manual = app
        .create(
            "manual",
            Status::InTransit,
            Some("2097-01-01T00:00:00Z"),
            None,
        )
        .await;
    app.store
        .set_override(
            manual.id,
            manual.revision,
            Some(SetOverride {
                status: Status::Delivered,
                reason: "Delivered locally".into(),
            }),
            "test",
        )
        .await
        .unwrap();
    sqlx::query("UPDATE shipments SET override_at=? WHERE id=?")
        .bind("2026-10-16T00:00:00Z")
        .bind(manual.id.to_string())
        .execute(&app.pool)
        .await
        .unwrap();
    let unknown_tied = app
        .create("unknown-tied", Status::Delivered, None, None)
        .await;
    app.create(
        "active",
        Status::InTransit,
        Some("2026-10-16T00:00:00Z"),
        None,
    )
    .await;
    let records = app
        .all(&[
            ("sort", "delivered"),
            ("status", "delivered"),
            ("limit", "2"),
        ])
        .await;
    assert_eq!(
        ids(&records),
        vec![
            manual.id,
            recent.id,
            tied.id,
            old.id,
            unknown.id,
            unknown_tied.id
        ]
    );
    assert!(
        records
            .iter()
            .all(|shipment| shipment.status == Status::Delivered)
    );
    let default_order = app.all(&[("limit", "2")]).await;
    assert!(ids(&default_order).windows(2).all(|pair| pair[0] < pair[1]));
    app.close().await;
}

#[tokio::test]
async fn canonical_carrier_catalog_requires_auth_and_filters_use_provider_names() {
    let app = App::new().await;
    let endpoint = format!("{}/v1/carriers", app.url);
    assert_eq!(
        app.client.get(&endpoint).send().await.unwrap().status(),
        StatusCode::UNAUTHORIZED
    );
    let carriers: Vec<Carrier> = app
        .client
        .get(&endpoint)
        .bearer_auth(OWNER)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(carriers, Carrier::ALL);
    let shipment = app.create("catalog", Status::InTransit, None, None).await;
    let records = app.all(&[("carrier", "USPS"), ("sort", "expected")]).await;
    assert_eq!(ids(&records), vec![shipment.id]);
    assert_eq!(
        serde_json::to_string(&records[0].tracking.carrier).unwrap(),
        "\"USPS\""
    );
    let rejected = app
        .client
        .get(format!("{}/v1/shipments", app.url))
        .bearer_auth(OWNER)
        .query(&[("carrier", "nonsense")])
        .send()
        .await
        .unwrap();
    assert_eq!(rejected.status(), StatusCode::BAD_REQUEST);
    app.close().await;
}
