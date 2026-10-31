//! Native projections preserve provider civil precision and actual scan priority.

use std::{collections::BTreeMap, sync::Arc};

use dropshot::ResultsPage;
use serde::{Deserialize, Serialize};
use shipments::{
    api,
    carrier::Carrier,
    models::{
        CarrierEvent, CarrierTracking, CreateShipment, Direction, Filters, InputTracking,
        SetOverride, Shipment, ShipmentSort, Source, Status, Summary,
    },
    shipping_api::types::{ProviderId, Tracker},
    store::Store,
};
use uuid::Uuid;

const OWNER: &str = "typed-delivery-owner-credential-at-least-32-bytes";

// The normalized provider boundary is deliberately independent of the public
// model additions so these exact regressions also compile against 0.6.23.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum CivilValue {
    Estimated { date: String },
    EstimatedRange { start: String, end: String },
    Delivered { date: String },
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
struct CivilDate {
    #[serde(flatten)]
    value: CivilValue,
    observed_at: String,
    timezone: String,
}

#[derive(Serialize)]
struct Observation {
    delivery_date: CivilDate,
    delivered_at: Option<&'static str>,
    estimated_delivery_at: Option<&'static str>,
    events: Vec<CarrierEvent>,
}

#[derive(Debug, Deserialize)]
struct CalendarItem {
    id: Uuid,
    title: String,
    delivery_date: Option<CivilDate>,
    estimated_delivery_at: Option<String>,
    delivered_at: Option<String>,
}

struct Fixture {
    directory: tempfile::TempDir,
    store: Store,
    rows: BTreeMap<String, Shipment>,
}

impl Fixture {
    async fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(&directory.path().join("typed-dates.sqlite3"))
            .await
            .unwrap();
        let mut rows = BTreeMap::new();
        let cases = [
            (
                "Civil estimate",
                Status::InTransit,
                CivilValue::Estimated {
                    date: "2026-11-01".into(),
                },
                None,
                None,
            ),
            (
                "Civil range",
                Status::InTransit,
                CivilValue::EstimatedRange {
                    start: "2026-11-02".into(),
                    end: "2026-11-04".into(),
                },
                None,
                None,
            ),
            (
                "Civil delivered",
                Status::Delivered,
                CivilValue::Delivered {
                    date: "2026-10-31".into(),
                },
                None,
                None,
            ),
            (
                "Actual timestamp",
                Status::Delivered,
                CivilValue::Delivered {
                    date: "2026-10-31".into(),
                },
                Some("2026-11-01T00:30:00+09:00"),
                None,
            ),
            (
                "Actual outside day",
                Status::Delivered,
                CivilValue::Delivered {
                    date: "2026-10-31".into(),
                },
                Some("2026-10-31T00:30:00+09:00"),
                None,
            ),
            (
                "Civil next day",
                Status::Delivered,
                CivilValue::Delivered {
                    date: "2026-11-01".into(),
                },
                None,
                None,
            ),
            (
                "Civil delivered with manual status",
                Status::Delivered,
                CivilValue::Delivered {
                    date: "2026-10-25".into(),
                },
                None,
                None,
            ),
            (
                "Canonical estimate",
                Status::InTransit,
                CivilValue::Estimated {
                    date: "2026-11-10".into(),
                },
                None,
                Some("2026-11-03T12:00:00Z"),
            ),
            (
                "Manual status is not delivery evidence",
                Status::InTransit,
                CivilValue::Estimated {
                    date: "2026-11-05".into(),
                },
                None,
                None,
            ),
            (
                "Civil delivery with later correction",
                Status::Delivered,
                CivilValue::Delivered {
                    date: "2026-10-31".into(),
                },
                None,
                None,
            ),
            (
                "Archived civil delivery",
                Status::Delivered,
                CivilValue::Delivered {
                    date: "2026-10-31".into(),
                },
                None,
                None,
            ),
            (
                "Outbound civil delivery",
                Status::Delivered,
                CivilValue::Delivered {
                    date: "2026-10-31".into(),
                },
                None,
                None,
            ),
        ];
        for (index, (title, status, value, delivered_at, estimated_delivery_at)) in
            cases.into_iter().enumerate()
        {
            let number = format!("9400111122223333{index:06}");
            let observation = Observation {
                delivery_date: CivilDate {
                    value,
                    observed_at: "2026-10-31T19:00:00Z".into(),
                    timezone: "America/Los_Angeles".into(),
                },
                delivered_at,
                estimated_delivery_at,
                events: vec![CarrierEvent {
                    status: Some(Status::InTransit),
                    datetime: Some("2026-10-24T12:00:00Z".into()),
                    message: Some("Synthetic retained carrier scan".into()),
                    ..Default::default()
                }],
            };
            let carrier_tracking: CarrierTracking =
                serde_json::from_str(&serde_json::to_string(&observation).unwrap()).unwrap();
            let tracker = Tracker {
                provider: ProviderId::AmazonAccount,
                id: format!("synthetic-account:package-{index}"),
                tracking_code: number.clone(),
                carrier: Carrier::Usps,
                status,
                public_url: None,
                carrier_tracking,
            };
            let mut row = store
                .create(
                    CreateShipment {
                        title: title.into(),
                        direction: if title.starts_with("Outbound") {
                            Direction::Outbound
                        } else {
                            Direction::Inbound
                        },
                        merchant: Some("Synthetic merchant".into()),
                        order_reference: Some(format!("synthetic-order-{index}")),
                        signature_required: Some(true),
                        tracking: InputTracking {
                            number,
                            carrier: Some(Carrier::Usps),
                            recipient_url: None,
                        },
                        source: Some(Source {
                            namespace: "typed-date-fixture".into(),
                            key: index.to_string(),
                        }),
                    },
                    "owner",
                    &format!("typed-date-{index}"),
                    Some(&tracker),
                )
                .await
                .unwrap();
            if title.contains("manual status")
                || title.starts_with("Manual status")
                || title.contains("later correction")
            {
                row = store
                    .set_override(
                        row.id,
                        row.revision,
                        Some(SetOverride {
                            status: if title.contains("later correction") {
                                Status::Failure
                            } else {
                                Status::Delivered
                            },
                            reason: "Synthetic owner correction".into(),
                        }),
                        "owner",
                    )
                    .await
                    .unwrap();
                // Pin only the fixture's manual set time, not a delivery scan.
                sqlx::query("UPDATE shipments SET override_at='2026-11-03T12:00:00Z' WHERE id=?")
                    .bind(row.id.to_string())
                    .execute(store.pool())
                    .await
                    .unwrap();
                row = store.get(row.id).await.unwrap();
            }
            if title.starts_with("Archived") {
                row = store
                    .archive(row.id, row.revision, true, "owner")
                    .await
                    .unwrap();
            }
            rows.insert(title.into(), row);
        }
        Self {
            directory,
            store,
            rows,
        }
    }

    async fn snapshot(&self) -> Vec<(String, String)> {
        let mut values = Vec::new();
        for row in self.rows.values() {
            values.push((
                serde_json::to_string(&self.store.get(row.id).await.unwrap()).unwrap(),
                serde_json::to_string(&self.store.events(row.id, 0, 200).await.unwrap()).unwrap(),
            ));
        }
        values
    }
}

fn at(value: &str) -> i64 {
    time::OffsetDateTime::parse(value, &time::format_description::well_known::Rfc3339)
        .unwrap()
        .unix_timestamp()
}

#[tokio::test]
async fn typed_provider_dates_flow_through_calendar_sorts_and_civil_day_queries() {
    let fixture = Fixture::new().await;
    let before = fixture.snapshot().await;
    let server = api::start(
        "127.0.0.1:0".parse().unwrap(),
        Arc::new(api::Context::new(fixture.store.clone(), OWNER, None, None)),
    )
    .unwrap();
    let origin = format!("http://{}", server.local_addr());
    let client = reqwest::Client::new();
    let mut cursor: Option<String> = None;
    let mut items = Vec::new();
    loop {
        let request = client
            .get(format!("{origin}/v1/delivery-estimates"))
            .bearer_auth(OWNER)
            .query(&[("limit", "1")]);
        let request = if let Some(cursor) = &cursor {
            request.query(&[("page_token", cursor)])
        } else {
            request
        };
        let response = request.send().await.unwrap();
        assert_eq!(response.status(), 200);
        let page: ResultsPage<CalendarItem> = response.json().await.unwrap();
        items.extend(page.items);
        cursor = page.next_page;
        if cursor.is_none() {
            break;
        }
    }
    let titles: Vec<_> = items.iter().map(|item| item.title.as_str()).collect();
    assert_eq!(
        titles,
        [
            "Civil delivered with manual status",
            "Actual outside day",
            "Civil delivered",
            "Actual timestamp",
            "Civil estimate",
            "Civil next day",
            "Civil range",
            "Canonical estimate",
        ]
    );
    for item in &items {
        let row = &fixture.rows[&item.title];
        assert_eq!(item.id, row.id);
        assert_eq!(item.delivered_at, row.carrier_tracking.delivered_at);
        assert_eq!(
            item.estimated_delivery_at,
            row.carrier_tracking.estimated_delivery_at
        );
        assert!(item.delivery_date.is_some());
    }
    let civil = items
        .iter()
        .find(|item| item.title == "Civil delivered")
        .unwrap();
    assert_eq!(
        civil.delivery_date.as_ref().unwrap().value,
        CivilValue::Delivered {
            date: "2026-10-31".into()
        }
    );
    assert!(
        civil.delivered_at.is_none(),
        "civil precision must not fabricate a scan instant"
    );
    assert_eq!(fixture.snapshot().await, before);
    server.close().await.unwrap();
    fixture.store.close().await.unwrap();
    if let Ok(path) = std::env::var("SHIPMENTS_TEST_TYPED_DATE_DATABASE") {
        std::fs::copy(fixture.directory.path().join("typed-dates.sqlite3"), path).unwrap();
    }
}

#[tokio::test]
async fn typed_provider_dates_participate_in_native_indexed_sorting_without_replacing_actual_times()
{
    let fixture = Fixture::new().await;
    let before = fixture.snapshot().await;
    let expected = fixture
        .store
        .list(
            &Filters {
                direction: Some(Direction::Inbound),
                status: Some(Status::InTransit),
                sort: Some(ShipmentSort::Expected),
                ..Default::default()
            },
            None,
            100,
        )
        .await
        .unwrap();
    assert_eq!(
        expected
            .iter()
            .map(|entry| entry.shipment.title.as_str())
            .collect::<Vec<_>>(),
        ["Civil estimate", "Civil range", "Canonical estimate"]
    );
    let filters = Filters {
        direction: Some(Direction::Inbound),
        status: Some(Status::Delivered),
        sort: Some(ShipmentSort::Delivered),
        ..Default::default()
    };
    let mut position = None;
    let mut delivered = Vec::new();
    loop {
        let page = fixture.store.list(&filters, position, 1).await.unwrap();
        let Some(entry) = page.into_iter().next() else {
            break;
        };
        delivered.push(entry.shipment.title);
        position = Some(entry.position);
    }
    assert_eq!(
        delivered,
        [
            "Manual status is not delivery evidence",
            "Civil next day",
            "Actual timestamp",
            "Civil delivered",
            "Actual outside day",
            "Civil delivered with manual status"
        ]
    );
    assert_eq!(fixture.snapshot().await, before);
    fixture.store.close().await.unwrap();
}

#[tokio::test]
async fn civil_delivery_day_matches_explicit_day_without_weakening_timestamp_filters() {
    let fixture = Fixture::new().await;
    let before = fixture.snapshot().await;
    let server = api::start(
        "127.0.0.1:0".parse().unwrap(),
        Arc::new(api::Context::new(fixture.store.clone(), OWNER, None, None)),
    )
    .unwrap();
    let origin = format!("http://{}", server.local_addr());
    let client = reqwest::Client::new();
    for (date, start, end, expected) in [
        (
            Some("2026-10-31"),
            "2026-10-31T00:00:00-07:00",
            "2026-11-01T00:00:00-07:00",
            vec![
                "Actual timestamp",
                "Civil delivered",
                "Civil delivery with later correction",
            ],
        ),
        (
            None,
            "2026-10-31T00:00:00-07:00",
            "2026-11-01T00:00:00-07:00",
            vec!["Actual timestamp"],
        ),
        // The caller's civil date is Nov1 even though its UTC start is Oct31.
        (
            Some("2026-11-01"),
            "2026-11-01T00:00:00+09:00",
            "2026-11-02T00:00:00+09:00",
            vec!["Civil next day", "Actual timestamp"],
        ),
    ] {
        let mut query = vec![
            ("direction", "inbound".to_owned()),
            ("sort", "delivered".to_owned()),
            ("delivered_after", at(start).to_string()),
            ("delivered_before", at(end).to_string()),
        ];
        if let Some(date) = date {
            query.push(("delivered_on", date.into()));
        }
        let mut token: Option<String> = None;
        let mut actual = Vec::new();
        loop {
            let request = client
                .get(format!("{origin}/v1/shipments"))
                .bearer_auth(OWNER)
                .query(&[("limit", "1")]);
            let request = if let Some(token) = &token {
                request.query(&[("page_token", token)])
            } else {
                request.query(&query)
            };
            let response = request.send().await.unwrap();
            assert_eq!(response.status(), 200);
            let page: ResultsPage<Shipment> = response.json().await.unwrap();
            actual.extend(page.items.into_iter().map(|item| item.title));
            token = page.next_page;
            if token.is_none() {
                break;
            }
        }
        assert_eq!(actual, expected);
        query.retain(|(key, _)| *key != "sort");
        let response = client
            .get(format!("{origin}/v1/summary"))
            .bearer_auth(OWNER)
            .query(&query)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let summary: Summary = response.json().await.unwrap();
        assert_eq!(summary.delivered, expected.len() as i64);
        assert_eq!(summary.exceptions, i64::from(date == Some("2026-10-31")));
    }
    for route in ["shipments", "summary"] {
        for query in [
            vec![("delivered_on", "2026-10-31".to_owned())],
            vec![
                ("delivered_on", "2026-02-30".to_owned()),
                ("delivered_after", "1793430000".into()),
                ("delivered_before", "1793516400".into()),
            ],
            vec![
                ("delivered_on", "2026-1-01".to_owned()),
                ("delivered_after", "1793430000".into()),
                ("delivered_before", "1793516400".into()),
            ],
        ] {
            assert_eq!(
                client
                    .get(format!("{origin}/v1/{route}"))
                    .bearer_auth(OWNER)
                    .query(&query)
                    .send()
                    .await
                    .unwrap()
                    .status(),
                400
            );
        }
    }
    assert_eq!(fixture.snapshot().await, before);
    server.close().await.unwrap();
    fixture.store.close().await.unwrap();
}
