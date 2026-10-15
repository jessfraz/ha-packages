use std::sync::Arc;

use dropshot::ResultsPage;
use reqwest::Client;
use shipments::{
    api,
    models::{CreateShipment, SetOverride, Shipment, Status, Tracking},
    store::Store,
};

#[tokio::test]
async fn exception_filter_uses_effective_status_search_and_keyset_pagination() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(&directory.path().join("exceptions.sqlite3"))
        .await
        .unwrap();
    let mut expected = Vec::new();
    for (index, status) in [
        Status::InTransit,
        Status::Failure,
        Status::Delivered,
        Status::Error,
        Status::ReturnToSender,
        Status::Cancelled,
    ]
    .into_iter()
    .enumerate()
    {
        let created = store
            .create(
                CreateShipment {
                    direction: shipments::models::Direction::Inbound,
                    title: format!("Filter parcel {index}"),
                    merchant: None,
                    order_reference: None,
                    signature_required: None,
                    tracking: Tracking {
                        carrier: Some("usps".parse().unwrap()),
                        number: format!("FILTER{index}"),
                    },
                    source: None,
                },
                "test",
                &format!("filter-{index}"),
                None,
            )
            .await
            .unwrap();
        let overridden = store
            .set_override(
                created.id,
                created.revision,
                Some(SetOverride {
                    status,
                    reason: "Filter test status".into(),
                }),
                "owner",
            )
            .await
            .unwrap();
        if matches!(
            status,
            Status::Failure | Status::Error | Status::ReturnToSender
        ) {
            expected.push(overridden.id);
        }
    }
    assert_eq!(
        store.summary(None).await.unwrap().exceptions,
        expected.len() as i64
    );
    let owner = "exceptions-owner-credential-at-least-32-bytes";
    let server = api::start(
        "127.0.0.1:0".parse().unwrap(),
        Arc::new(api::Context::new(store.clone(), owner, None, None)),
    )
    .unwrap();
    let base = format!("http://{}/v1/shipments", server.local_addr());
    let client = Client::new();
    let mut found = Vec::new();
    let mut token: Option<String> = None;
    loop {
        let request = client
            .get(&base)
            .bearer_auth(owner)
            .query(&[("limit", "1")]);
        let request = match &token {
            Some(token) => request.query(&[("page_token", token)]),
            None => request.query(&[("exceptions", "true"), ("search", "Filter parcel")]),
        };
        let page: ResultsPage<Shipment> = request.send().await.unwrap().json().await.unwrap();
        assert_eq!(page.items.len(), 1);
        found.push(page.items[0].id);
        let Some(next) = page.next_page else {
            break;
        };
        token = Some(next);
    }
    assert_eq!(found, expected);
    let normal: ResultsPage<Shipment> = client
        .get(&base)
        .bearer_auth(owner)
        .query(&[("exceptions", "false")])
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(normal.items.len(), 3);
    assert!(
        normal
            .items
            .iter()
            .all(|shipment| !expected.contains(&shipment.id))
    );
    let searched: ResultsPage<Shipment> = client
        .get(&base)
        .bearer_auth(owner)
        .query(&[("exceptions", "true"), ("search", "parcel 3")])
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(searched.items.len(), 1);
    assert_eq!(searched.items[0].status, Status::Error);
    server.close().await.unwrap();
    store.close().await.unwrap();
}
