use shipments::{api, store::Store};
use std::sync::Arc;

#[tokio::test]
async fn api_requires_tracking_before_any_provider_access() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(&directory.path().join("shipments.sqlite3"))
        .await
        .unwrap();
    let token = "disposable-owner-credential-for-required-tracking";
    let context = Arc::new(api::Context::new(store.clone(), token, None, None));
    let server = api::start("127.0.0.1:0".parse().unwrap(), context).unwrap();
    let client = reqwest::Client::new();
    for body in [
        r#"{"direction":"inbound","title":"Parcel"}"#,
        r#"{"direction":"inbound","title":"Parcel","tracking":null}"#,
        r#"{"direction":"inbound","title":"Parcel","tracking":{"number":"  "}}"#,
    ] {
        let response = client
            .post(format!("http://{}/v1/shipments", server.local_addr()))
            .bearer_auth(token)
            .header("Idempotency-Key", "required-tracking")
            .header("Content-Type", "application/json")
            .body(body)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::BAD_REQUEST);
        let error: Failure = response.json().await.unwrap();
        assert_eq!(error.error_code.as_deref(), Some("tracking_required"));
        assert!(error.message.contains("tracking number is required"));
    }
    server.close().await.unwrap();
    store.close().await.unwrap();
}

#[derive(serde::Deserialize)]
struct Failure {
    error_code: Option<String>,
    message: String,
}

#[tokio::test]
async fn persistence_requires_a_resolved_carrier_and_applies_reverification() {
    use shipments::{
        error::Error,
        models::{CreateShipment, InputTracking, Patch, Status, UpdateShipment},
        shipping_api::types::Tracker,
    };
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(&directory.path().join("carrier.sqlite3"))
        .await
        .unwrap();
    let input = CreateShipment {
        title: "Detect the carrier".into(),
        direction: Default::default(),
        merchant: None,
        order_reference: None,
        signature_required: None,
        source: None,
        tracking: InputTracking {
            recipient_url: None,
            number: "9400100000000000000001".into(),
            carrier: None,
        },
    };
    assert!(matches!(
        store.create(input.clone(), "owner", "detect", None).await,
        Err(Error::CarrierRequired)
    ));
    let counts: (i64, i64) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM shipments),(SELECT count(*) FROM idempotency)",
    )
    .fetch_one(store.pool())
    .await
    .unwrap();
    assert_eq!(counts, (0, 0));
    let tracker = Tracker {
        provider: shipments::shipping_api::types::ProviderId::EasyPost,
        id: "trk_detected".into(),
        tracking_code: input.tracking.number.clone(),
        carrier: "USPS".parse().unwrap(),
        status: Status::Unknown,
        public_url: None,
        carrier_tracking: Default::default(),
    };
    let saved = store
        .create(input, "owner", "detect", Some(&tracker))
        .await
        .unwrap();
    assert_eq!(saved.tracking.carrier, tracker.carrier);
    assert_eq!(saved.tracking.number, tracker.tracking_code);
    let update = UpdateShipment {
        tracking: Patch {
            value: Some(Some(InputTracking {
                recipient_url: None,
                carrier: None,
                number: "9400100000000000000002".into(),
            })),
        },
        ..Default::default()
    };
    assert!(matches!(
        store
            .update(saved.id, saved.revision, update, "owner", None)
            .await,
        Err(Error::CarrierRequired)
    ));
    let preserved = store.get(saved.id).await.unwrap();
    assert_eq!(preserved.tracking, saved.tracking);
    assert_eq!(preserved.revision, saved.revision);
    sqlx::query("UPDATE shipments SET registration_state='rejected',last_error='Configure carrier',last_error_code='tracking_carrier_credentials_required',next_attempt_at=NULL WHERE id=?")
        .bind(saved.id.to_string()).execute(store.pool()).await.unwrap();
    let blocked = store.get(saved.id).await.unwrap();
    assert_eq!(
        blocked.sync.registration_state,
        shipments::models::RegistrationState::ConfigurationRequired
    );
    let restored = store
        .update(
            saved.id,
            blocked.revision,
            UpdateShipment {
                tracking: Patch {
                    value: Some(Some(InputTracking {
                        recipient_url: None,
                        number: saved.tracking.number.clone(),
                        carrier: None,
                    })),
                },
                ..Default::default()
            },
            "owner",
            Some(&tracker),
        )
        .await
        .unwrap();
    assert_eq!(restored.tracking, saved.tracking);
    assert_eq!(
        restored.sync.registration_state,
        shipments::models::RegistrationState::Registered
    );
    assert!(restored.sync.last_error.is_none());
    assert!(restored.sync.next_attempt_at.is_some());
    store.close().await.unwrap();
}

#[test]
fn input_allows_detection_but_stored_tracking_requires_a_carrier() {
    use shipments::models::{InputTracking, Tracking};
    for body in [
        r#"{"number":"9400100000000000000001"}"#,
        r#"{"number":"9400100000000000000001","carrier":null}"#,
    ] {
        let input: InputTracking = serde_json::from_str(body).unwrap();
        assert!(input.carrier.is_none());
        assert!(serde_json::from_str::<Tracking>(body).is_err());
    }
}
