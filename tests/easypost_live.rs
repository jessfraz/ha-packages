//! Opt-in coverage of the real EasyPost test API through Shipments' HTTP API.
//! Only synthetic tracking codes and an EZTK test key are used.

use std::{path::Path, sync::Arc, time::Duration};

use dropshot::{HttpServer, ResultsPage};
use reqwest::{Client, Method, StatusCode};
use shipments::{
    api,
    carrier::Carrier,
    models::{
        CreateShipment, Direction, InputTracking, Patch, RegistrationState, Shipment, Status,
        UpdateShipment,
    },
    shipping_api::easypost::EasyPostClient,
    store::Store,
};

const OWNER: &str = "sandbox-owner-credential-at-least-32-bytes";

struct App {
    store: Store,
    server: HttpServer<Arc<api::Context>>,
    client: Client,
    url: String,
}

impl App {
    async fn start(database: &Path, provider: Arc<EasyPostClient>) -> Self {
        let store = Store::open(database).await.unwrap();
        let server = api::start(
            "127.0.0.1:0".parse().unwrap(),
            Arc::new(api::Context::new(
                store.clone(),
                OWNER,
                None,
                Some(provider),
            )),
        )
        .unwrap();
        let url = format!("http://{}", server.local_addr());
        Self {
            store,
            server,
            url,
            client: Client::builder()
                .timeout(Duration::from_secs(60))
                .build()
                .unwrap(),
        }
    }

    fn request(&self, method: Method, path: &str) -> reqwest::RequestBuilder {
        self.client
            .request(method, format!("{}{path}", self.url))
            .bearer_auth(OWNER)
    }

    async fn close(self) {
        self.server.close().await.unwrap();
        self.store.close().await.unwrap();
    }
}

fn input(number: &str, carrier: Option<Carrier>) -> CreateShipment {
    CreateShipment {
        title: "EasyPost synthetic test parcel".into(),
        direction: Direction::Inbound,
        merchant: None,
        order_reference: None,
        signature_required: None,
        tracking: InputTracking {
            recipient_url: None,
            number: number.into(),
            carrier,
        },
        source: None,
    }
}

#[tokio::test]
#[ignore = "requires EASYPOST_TEST_API_KEY and the live EasyPost test API"]
async fn easypost_sandbox_admission_persistence_and_replay() {
    let key = std::env::var("EASYPOST_TEST_API_KEY")
        .expect("Set EASYPOST_TEST_API_KEY to run the EasyPost sandbox test");
    assert!(
        key.starts_with("EZTK"),
        "Refusing provider requests without an EasyPost EZTK test key"
    );
    let provider = Arc::new(EasyPostClient::new(key).unwrap());
    provider.check_connection().await.unwrap();
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("shipments.sqlite3");
    let app = App::start(&database, provider.clone()).await;
    let mut saved = Vec::new();

    // These are EasyPost's documented fixtures, never real package numbers.
    // Unknown and pre-transit remain valid before the first useful scan.
    for (number, carrier, expected_carrier, status) in [
        ("EZ1000000001", None, Carrier::Usps, Status::PreTransit),
        (
            "EZ3000000003",
            Some(Carrier::Ups),
            Carrier::Ups,
            Status::OutForDelivery,
        ),
        (
            "EZ4000000004",
            Some(Carrier::FedExDefault),
            Carrier::FedExDefault,
            Status::Delivered,
        ),
        (
            "EZ7000000007",
            Some(Carrier::Usps),
            Carrier::Usps,
            Status::Unknown,
        ),
    ] {
        // Always exercise the real provider POST, even when CI has run before.
        // EasyPost reuses these fixed test trackers. Admission must adopt them.
        let registered = provider.register(number, carrier).await.unwrap();
        let body = input(number, carrier);
        let response = app
            .request(Method::POST, "/v1/shipments")
            .header("Idempotency-Key", number)
            .json(&body)
            .send()
            .await
            .unwrap();
        let http_status = response.status();
        let response_body = response.text().await.unwrap();
        assert_eq!(http_status, StatusCode::CREATED, "{response_body}");
        let shipment: Shipment = serde_json::from_str(&response_body).unwrap();
        assert_eq!(shipment.tracking.number, number);
        assert_eq!(shipment.tracking.carrier, expected_carrier);
        assert_eq!(shipment.status, status);
        assert_eq!(
            shipment.sync.registration_state,
            RegistrationState::Registered
        );
        assert!(shipment.sync.last_error.is_none());
        let tracker_id = shipment.sync.tracker_id.as_ref().unwrap();
        assert_eq!(tracker_id, &registered.id);
        let observation = provider.retrieve(tracker_id, number).await.unwrap();
        assert_eq!(observation.status, status);
        assert_eq!(observation.carrier, expected_carrier);
        if status == Status::Delivered {
            assert!(shipment.carrier_tracking.delivered_at.is_some());
            assert!(!shipment.carrier_tracking.events.is_empty());
        }
        saved.push((body, shipment));
    }
    app.close().await;

    // Reopen SQLite and replay the original requests through the real API.
    let app = App::start(&database, provider).await;
    for (body, original) in &saved {
        let response = app
            .request(Method::POST, "/v1/shipments")
            .header("Idempotency-Key", &body.tracking.number)
            .json(body)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
        let replay: Shipment = response.json().await.unwrap();
        assert_eq!(replay.id, original.id);
        assert_eq!(replay.sync.tracker_id, original.sync.tracker_id);
        assert_eq!(replay.tracking, original.tracking);
    }

    // A provider-rejected synthetic code must save nothing, including on PATCH.
    let invalid = input("EZ0000000000", Some(Carrier::Usps));
    let response = app
        .request(Method::POST, "/v1/shipments")
        .header("Idempotency-Key", "invalid-test-code")
        .json(&invalid)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    #[derive(serde::Deserialize)]
    struct Failure {
        error_code: String,
        message: String,
    }
    let failure: Failure = response.json().await.unwrap();
    assert_eq!(failure.error_code, "tracking_test_key");
    assert!(failure.message.contains("No shipment was saved or changed"));

    let original = &saved[0].1;
    let path = format!("/v1/shipments/{}", original.id);
    let response = app
        .request(Method::PATCH, &path)
        .header("If-Match", format!("\"{}\"", original.revision))
        .json(&UpdateShipment {
            tracking: Patch {
                value: Some(Some(input("EZ2000000002", None).tracking)),
            },
            ..Default::default()
        })
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let updated: Shipment = response.json().await.unwrap();
    assert_eq!(updated.tracking.number, "EZ2000000002");
    assert_eq!(updated.tracking.carrier, Carrier::Usps);
    assert_eq!(updated.status, Status::InTransit);
    assert_eq!(
        updated.sync.registration_state,
        RegistrationState::Registered
    );

    let response = app
        .request(Method::PATCH, &path)
        .header("If-Match", format!("\"{}\"", updated.revision))
        .json(&UpdateShipment {
            tracking: Patch {
                value: Some(Some(input("EZ0000000008", Some(Carrier::Usps)).tracking)),
            },
            ..Default::default()
        })
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let failure: Failure = response.json().await.unwrap();
    assert_eq!(failure.error_code, "tracking_test_key");
    let unchanged: Shipment = app
        .request(Method::GET, &path)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(unchanged.tracking, updated.tracking);
    assert_eq!(unchanged.revision, updated.revision);
    assert_eq!(unchanged.sync.tracker_id, updated.sync.tracker_id);
    let page: ResultsPage<Shipment> = app
        .request(Method::GET, "/v1/shipments?limit=100")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(page.items.len(), saved.len());
    assert!(page.next_page.is_none());
    app.close().await;
}
