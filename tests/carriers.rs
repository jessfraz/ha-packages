use std::sync::Arc;

use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use shipments::{api, error::Error, models::CreateShipment, store::Store};

#[derive(Serialize)]
struct UnvalidatedTracking<'a> {
    number: &'a str,
    carrier: &'a str,
}

#[derive(Serialize)]
struct UnvalidatedShipment<'a> {
    title: &'a str,
    direction: shipments::models::Direction,
    tracking: UnvalidatedTracking<'a>,
}

#[derive(Deserialize)]
struct Rejection {
    message: String,
}

#[tokio::test]
async fn unsupported_carrier_is_rejected_before_provider_configuration_is_required() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(&directory.path().join("shipments.sqlite3"))
        .await
        .unwrap();
    let owner = "carrier-test-owner-credential-at-least-32-bytes";
    let server = api::start(
        "127.0.0.1:0".parse().unwrap(),
        Arc::new(api::Context::new(store.clone(), owner, None, None)),
    )
    .unwrap();
    let response = reqwest::Client::new()
        .post(format!("http://{}/v1/shipments", server.local_addr()))
        .bearer_auth(owner)
        .header("Idempotency-Key", "unsupported-carrier")
        .json(&UnvalidatedShipment {
            title: "Carrier validation parcel",
            direction: shipments::models::Direction::Inbound,
            tracking: UnvalidatedTracking {
                number: "TRACK-UNSUPPORTED-CARRIER",
                carrier: "UnrecognizedCarrier",
            },
        })
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let rejection: Rejection = response.json().await.unwrap();
    assert!(rejection.message.contains("Unsupported tracking carrier"));
    assert!(rejection.message.contains("/v1/carriers"));
    assert!(rejection.message.contains("automatic detection"));
    server.close().await.unwrap();
    store.close().await.unwrap();
}

#[tokio::test]
async fn canonical_carriers_preserve_legacy_create_replays_and_database_identity() {
    // SHA-256 of the normalized 0.3.1 CreateShipment, before signature metadata
    // and canonical carrier serialization were added.
    let legacy_fingerprint = "7544857e9aaff2976b141e1cb728de5f1133ff55fcaac7302e8b257e469fa71c";
    let mut input: CreateShipment = serde_json::from_str(r#"{"title":"Legacy parcel","direction":"inbound","merchant":null,"order_reference":null,"tracking":{"carrier":"usps","number":"LEGACY-TRACK"},"source":null}"#).unwrap();
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(&directory.path().join("shipments.sqlite3"))
        .await
        .unwrap();
    let created = store
        .create(input.clone(), "legacy-agent", "legacy-key", None)
        .await
        .unwrap();
    let fingerprint: String =
        sqlx::query_scalar("SELECT fingerprint FROM idempotency WHERE actor_id=? AND key=?")
            .bind("legacy-agent")
            .bind("legacy-key")
            .fetch_one(store.pool())
            .await
            .unwrap();
    assert_eq!(fingerprint, legacy_fingerprint);
    let carrier: String = sqlx::query_scalar("SELECT tracking_carrier FROM shipments WHERE id=?")
        .bind(created.id.to_string())
        .fetch_one(store.pool())
        .await
        .unwrap();
    assert_eq!(carrier, "usps");
    let replay = store
        .replay_create(&input, "legacy-agent", "legacy-key")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(replay.id, created.id);
    input.signature_required = Some(false);
    assert!(matches!(
        store
            .replay_create(&input, "legacy-agent", "legacy-key")
            .await,
        Err(Error::Conflict(_))
    ));
    store.close().await.unwrap();
}
