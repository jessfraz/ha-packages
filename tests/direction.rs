use std::sync::Arc;

use dropshot::ResultsPage;
use reqwest::Client;
use shipments::{
    api,
    models::{CreateShipment, Direction, SetOverride, Shipment, Status, Summary, Tracking},
    store::Store,
};

#[tokio::test]
async fn direction_scopes_summaries_search_and_paginated_status_filters() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(&directory.path().join("direction.sqlite3"))
        .await
        .unwrap();
    let mut incoming = Vec::new();
    for (index, (direction, status, archived, failed)) in [
        (Direction::Inbound, Status::InTransit, false, true),
        (Direction::Outbound, Status::OutForDelivery, false, true),
        (Direction::Inbound, Status::Delivered, false, false),
        (Direction::Outbound, Status::Delivered, false, false),
        (Direction::Inbound, Status::Failure, false, false),
        (Direction::Outbound, Status::Error, false, false),
        (Direction::Inbound, Status::OutForDelivery, true, true),
        (Direction::Outbound, Status::OutForDelivery, true, true),
    ]
    .into_iter()
    .enumerate()
    {
        let created = store
            .create(
                CreateShipment {
                    title: format!("Direction parcel {index}"),
                    direction,
                    merchant: None,
                    order_reference: None,
                    signature_required: None,
                    tracking: Tracking {
                        carrier: Some("usps".parse().unwrap()),
                        number: format!("DIRECTION{index}"),
                    },
                    source: None,
                },
                "test",
                &format!("direction-{index}"),
                None,
            )
            .await
            .unwrap();
        let updated = store
            .set_override(
                created.id,
                created.revision,
                Some(SetOverride {
                    status,
                    reason: "Direction filter fixture".into(),
                }),
                "owner",
            )
            .await
            .unwrap();
        if archived {
            store
                .archive(updated.id, updated.revision, true, "owner")
                .await
                .unwrap();
        }
        if failed {
            sqlx::query("UPDATE shipments SET last_error='Temporary provider error' WHERE id=?")
                .bind(updated.id.to_string())
                .execute(store.pool())
                .await
                .unwrap();
        }
        if direction == Direction::Inbound && !archived {
            incoming.push(updated.id);
        }
    }
    let owner = "direction-test-owner-credential-at-least-32-bytes";
    let server = api::start(
        "127.0.0.1:0".parse().unwrap(),
        Arc::new(api::Context::new(store.clone(), owner, None, None)),
    )
    .unwrap();
    let base = format!("http://{}", server.local_addr());
    let client = Client::new();
    for (direction, out_for_delivery, active, delivered, errors) in [
        (Some("inbound"), 0, 2, 1, 1),
        (Some("outbound"), 1, 2, 1, 1),
        (None, 1, 4, 2, 2),
    ] {
        let request = client.get(format!("{base}/v1/summary")).bearer_auth(owner);
        let request = if let Some(direction) = direction {
            request.query(&[("direction", direction)])
        } else {
            request
        };
        let summary: Summary = request.send().await.unwrap().json().await.unwrap();
        assert_eq!(summary.active, active);
        assert_eq!(summary.out_for_delivery, out_for_delivery);
        assert_eq!(summary.delivered, delivered);
        assert_eq!(summary.exceptions, delivered);
        assert_eq!(summary.sync_errors, errors);
    }
    let mut ids = Vec::new();
    let mut token: Option<String> = None;
    loop {
        let request = client
            .get(format!("{base}/v1/shipments"))
            .bearer_auth(owner)
            .query(&[("limit", "1")]);
        let request = match &token {
            Some(token) => request.query(&[("page_token", token)]),
            None => request.query(&[
                ("direction", "inbound"),
                ("sort", "expected"),
                ("search", "Direction parcel"),
            ]),
        };
        let page: ResultsPage<Shipment> = request.send().await.unwrap().json().await.unwrap();
        assert_eq!(page.items.len(), 1);
        assert_eq!(page.items[0].direction, Direction::Inbound);
        ids.push(page.items[0].id);
        let Some(next) = page.next_page else {
            break;
        };
        token = Some(next);
    }
    assert_eq!(ids, incoming);
    let outgoing: ResultsPage<Shipment> = client
        .get(format!("{base}/v1/shipments"))
        .bearer_auth(owner)
        .query(&[("direction", "outbound"), ("status", "out_for_delivery")])
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(outgoing.items.len(), 1);
    assert_eq!(outgoing.items[0].direction, Direction::Outbound);
    let archived: ResultsPage<Shipment> = client
        .get(format!("{base}/v1/shipments"))
        .bearer_auth(owner)
        .query(&[("direction", "inbound"), ("archived", "true")])
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(archived.items.len(), 1);
    server.close().await.unwrap();
    store.close().await.unwrap();
}
