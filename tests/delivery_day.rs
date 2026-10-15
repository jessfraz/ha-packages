use std::sync::Arc;

use dropshot::ResultsPage;
use shipments::{
    api,
    carrier::Carrier,
    delivery::DeliveryWindow,
    models::{
        CarrierTracking, CreateShipment, Direction, Filters, InputTracking, SetOverride, Shipment,
        Status, Summary,
    },
    store::Store,
};

#[tokio::test]
async fn actual_delivery_window_is_half_open_inbound_and_independent_of_manual_status() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(&directory.path().join("delivery-day.sqlite3"))
        .await
        .unwrap();
    let mut expected = Vec::new();
    for (index, (direction, delivered_at, manual_status, archived, included)) in [
        (
            Direction::Inbound,
            Some("2026-10-15T06:59:59Z"),
            None,
            false,
            false,
        ),
        (
            Direction::Inbound,
            Some("2026-10-15T00:00:00-07:00"),
            None,
            false,
            true,
        ),
        (
            Direction::Inbound,
            Some("2026-10-15T14:00:00Z"),
            Some(Status::Failure),
            false,
            true,
        ),
        (
            Direction::Inbound,
            Some("2026-10-16T07:00:00Z"),
            None,
            false,
            false,
        ),
        (
            Direction::Outbound,
            Some("2026-10-15T14:00:00Z"),
            None,
            false,
            false,
        ),
        (
            Direction::Inbound,
            None,
            Some(Status::Delivered),
            false,
            false,
        ),
        (
            Direction::Inbound,
            Some("2026-10-15T14:00:00Z"),
            None,
            true,
            false,
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let created = store
            .create(
                CreateShipment {
                    title: format!("Delivery boundary {index}"),
                    direction,
                    merchant: None,
                    order_reference: None,
                    signature_required: None,
                    tracking: InputTracking {
                        recipient_url: None,
                        carrier: Some(Carrier::Usps),
                        number: format!("DAYBOUNDARY{index}"),
                    },
                    source: None,
                },
                "test",
                &format!("day-boundary-{index}"),
                None,
            )
            .await
            .unwrap();
        let tracking = CarrierTracking {
            delivered_at: delivered_at.map(str::to_owned),
            ..CarrierTracking::default()
        };
        sqlx::query("UPDATE shipments SET carrier_tracking=?,carrier_status=? WHERE id=?")
            .bind(serde_json::to_string(&tracking).unwrap())
            .bind(serde_json::to_string(&Status::Delivered).unwrap())
            .bind(created.id.to_string())
            .execute(store.pool())
            .await
            .unwrap();
        let mut current = store.get(created.id).await.unwrap();
        if let Some(status) = manual_status {
            current = store
                .set_override(
                    current.id,
                    current.revision,
                    Some(SetOverride {
                        status,
                        reason: "Test actual delivery independently of a correction".into(),
                    }),
                    "owner",
                )
                .await
                .unwrap();
        }
        if archived {
            store
                .archive(current.id, current.revision, true, "owner")
                .await
                .unwrap();
        }
        if included {
            expected.push(created.id);
        }
    }
    let after = 1_792_047_600;
    let before = 1_792_134_000;
    let window = DeliveryWindow::from_bounds(Some(after), Some(before)).unwrap();
    let summary = store
        .summary_in_window(Some(Direction::Inbound), window)
        .await
        .unwrap();
    assert_eq!(summary.delivered, 2);
    assert_eq!(summary.exceptions, 1);
    let combined = store.summary_in_window(None, window).await.unwrap();
    assert_eq!(combined.delivered, 3);
    assert_eq!(
        store
            .summary(Some(Direction::Inbound))
            .await
            .unwrap()
            .delivered,
        4
    );
    let filters = Filters {
        direction: Some(Direction::Inbound),
        delivered_after: Some(after),
        delivered_before: Some(before),
        ..Filters::default()
    };
    let mut position = None;
    let mut actual = Vec::new();
    loop {
        let page = store.list(&filters, position, 1).await.unwrap();
        if page.is_empty() {
            break;
        }
        assert_eq!(page.len(), 1);
        let entry = page.into_iter().next().unwrap();
        actual.push(entry.shipment.id);
        position = Some(entry.position);
    }
    assert_eq!(actual, expected);
    let owner = "day-boundary-owner-credential-at-least-32-bytes";
    let server = api::start(
        "127.0.0.1:0".parse().unwrap(),
        Arc::new(api::Context::new(store.clone(), owner, None, None)),
    )
    .unwrap();
    let origin = format!("http://{}", server.local_addr());
    let client = reqwest::Client::new();
    let response = client
        .get(format!("{origin}/v1/summary"))
        .bearer_auth(owner)
        .query(&[
            ("direction", "inbound"),
            ("delivered_after", &after.to_string()),
            ("delivered_before", &before.to_string()),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let summary: Summary = response.json().await.unwrap();
    assert_eq!(summary.delivered, 2);
    assert_eq!(summary.exceptions, 1);
    let mut token: Option<String> = None;
    let mut visible = Vec::new();
    loop {
        let request = client
            .get(format!("{origin}/v1/shipments"))
            .bearer_auth(owner)
            .query(&[("limit", "1")]);
        let request = if let Some(token) = &token {
            request.query(&[("page_token", token)])
        } else {
            request.query(&[
                ("direction", "inbound"),
                ("sort", "delivered"),
                ("delivered_after", &after.to_string()),
                ("delivered_before", &before.to_string()),
            ])
        };
        let response = request.send().await.unwrap();
        assert_eq!(response.status(), 200);
        let page: ResultsPage<Shipment> = response.json().await.unwrap();
        visible.extend(page.items.into_iter().map(|item| item.id));
        match page.next_page {
            Some(next) => token = Some(next),
            None => break,
        }
    }
    expected.reverse();
    assert_eq!(visible, expected);
    for route in ["shipments", "summary"] {
        let response = client
            .get(format!("{origin}/v1/{route}"))
            .bearer_auth(owner)
            .query(&[("delivered_after", after.to_string())])
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 400);
    }
    server.close().await.unwrap();
    let invalid = Filters {
        delivered_after: Some(after),
        ..Filters::default()
    };
    assert!(store.list(&invalid, None, 100).await.is_err());
}
