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
        r#"{"title":"Parcel"}"#,
        r#"{"title":"Parcel","tracking":null}"#,
        r#"{"title":"Parcel","tracking":{"number":"  "}}"#,
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
