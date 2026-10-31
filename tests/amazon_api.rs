use std::{
    io::Write,
    sync::{Arc, Mutex},
};

use reqwest::{Client, Method, StatusCode};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use shipments::{amazon::AmazonClient, api, logging, models::System, store::Store};

const OWNER: &str = "synthetic-amazon-api-owner-credential-32-bytes";
const AGENT: &str = "synthetic-amazon-agent-credential-32-bytes";
const INTEGRATION: &str = "synthetic-amazon-integration-credential-32-bytes";

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);
impl Write for Capture {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
#[derive(Deserialize)]
struct SignIn {
    flow_id: String,
    url: String,
}

#[tokio::test]
async fn account_management_requires_owner_and_keeps_one_time_data_out_of_health_and_logs() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(&directory.path().join("registry.sqlite3"))
        .await
        .unwrap();
    let amazon = Arc::new(AmazonClient::new(directory.path().join("amazon.json")).unwrap());
    store
        .create_agent("Agent", &Sha256::digest(AGENT.as_bytes()).into())
        .await
        .unwrap();
    let capture = Capture::default();
    let context = api::Context::new(store.clone(), OWNER, None, None)
        .with_integration(INTEGRATION)
        .with_amazon(amazon);
    let server = api::start_with_logger(
        "127.0.0.1:0".parse().unwrap(),
        Arc::new(context),
        logging::with_writer(capture.clone()),
    )
    .unwrap();
    let base = format!("http://{}", server.local_addr());
    let client = Client::new();
    let paths = [
        (Method::GET, "/v1/amazon"),
        (Method::POST, "/v1/amazon/sign-in"),
        (Method::POST, "/v1/amazon/complete"),
        (Method::POST, "/v1/amazon/preview"),
        (Method::POST, "/v1/amazon/enable"),
        (Method::POST, "/v1/amazon/refresh"),
        (Method::DELETE, "/v1/amazon"),
    ];
    for (method, path) in paths {
        for token in [None, Some(AGENT), Some(INTEGRATION)] {
            let mut request = client.request(method.clone(), format!("{base}{path}"));
            if let Some(token) = token {
                request = request.bearer_auth(token);
            }
            if path.ends_with("complete") {
                request=request.json(&serde_json::json!({"flow_id":"none","landing_url":"PRIVATE-CALLBACK-SENTINEL"}));
            }
            let response = request.send().await.unwrap();
            assert_eq!(
                response.status(),
                if token.is_none() {
                    StatusCode::UNAUTHORIZED
                } else {
                    StatusCode::FORBIDDEN
                },
                "{path}"
            );
        }
    }
    let asset = client
        .get(format!("{base}/amazon-settings.js"))
        .send()
        .await
        .unwrap();
    assert_eq!(asset.status(), StatusCode::OK);
    assert!(
        asset.headers()["content-type"]
            .to_str()
            .unwrap()
            .contains("javascript")
    );
    assert_eq!(asset.headers()["cache-control"], "no-store");
    let response = client
        .get(format!("{base}/v1/amazon"))
        .bearer_auth(OWNER)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["cache-control"], "no-store");
    let status: shipments::amazon::AmazonStatus = response.json().await.unwrap();
    assert_eq!(status.state, shipments::amazon::AmazonState::Disconnected);
    assert!(!status.imports_enabled);
    let response = client
        .post(format!("{base}/v1/amazon/sign-in"))
        .bearer_auth(OWNER)
        .header("X-Shipments-Actor", "owner-a")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["cache-control"], "no-store");
    let flow: SignIn = response.json().await.unwrap();
    let url = reqwest::Url::parse(&flow.url).unwrap();
    assert_eq!(url.origin().ascii_serialization(), "https://www.amazon.com");
    assert_eq!(url.path(), "/ap/signin");
    assert!(
        url.query_pairs()
            .any(|(k, v)| k == "openid.oa2.code_challenge_method" && v == "S256")
    );
    assert!(!url.query_pairs().any(|(k, _)| k.contains("verifier")));
    // A different owner cannot consume the flow, even with a valid-looking callback.
    let callback = "https://www.amazon.com/ap/maplanding?openid.oa2.authorization_code=PRIVATE-AUTHORIZATION-CODE";
    let response = client
        .post(format!("{base}/v1/amazon/complete"))
        .bearer_auth(OWNER)
        .header("X-Shipments-Actor", "owner-b")
        .json(&serde_json::json!({"flow_id":flow.flow_id,"landing_url":callback}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let response=client.post(format!("{base}/v1/amazon/complete")).bearer_auth(OWNER).header("X-Shipments-Actor","owner-a")
        .json(&serde_json::json!({"flow_id":flow.flow_id,"landing_url":"https://not-amazon.invalid/ap/maplanding?openid.oa2.authorization_code=PRIVATE-AUTHORIZATION-CODE"})).send().await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(
        !response
            .text()
            .await
            .unwrap()
            .contains("PRIVATE-AUTHORIZATION-CODE")
    );
    for path in ["preview", "enable"] {
        let response = client
            .post(format!("{base}/v1/amazon/{path}"))
            .bearer_auth(OWNER)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }
    let response = client
        .get(format!("{base}/v1/system"))
        .bearer_auth(INTEGRATION)
        .send()
        .await
        .unwrap();
    let body = response.text().await.unwrap();
    let system: System = serde_json::from_str(&body).unwrap();
    assert_eq!(
        system.amazon.state,
        shipments::amazon::AmazonState::Disconnected
    );
    assert!(
        !system
            .enabled_providers
            .contains(&shipments::shipping_api::types::ProviderId::AmazonAccount)
    );
    assert!(!system.provider_enabled);
    for secret in [
        "PRIVATE-AUTHORIZATION-CODE",
        "PRIVATE-CALLBACK-SENTINEL",
        flow.flow_id.as_str(),
        "code_verifier",
        "refresh_token",
    ] {
        assert!(!body.contains(secret));
    }
    server.close().await.unwrap();
    store.close().await.unwrap();
    let log = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
    for secret in [
        OWNER,
        INTEGRATION,
        AGENT,
        "PRIVATE-AUTHORIZATION-CODE",
        "PRIVATE-CALLBACK-SENTINEL",
        flow.flow_id.as_str(),
        "code_challenge",
    ] {
        assert!(
            !log.contains(secret),
            "Sensitive data appeared in captured logs"
        );
    }
    assert!(!directory.path().join("amazon.json").exists());
}

#[tokio::test]
async fn account_management_waits_for_disconnect_database_pause() {
    use std::{io::Write, os::unix::fs::OpenOptionsExt, time::Duration};
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("amazon.json");
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .unwrap();
    file.write_all(br#"{"refresh_token":"SYNTHETIC_REFRESH","device_serial":"SYNTHETIC_SERIAL","customer_id":"SYNTHETIC_CUSTOMER","generation":1,"imports_enabled":true}"#).unwrap();
    let account = Arc::new(AmazonClient::new(path.clone()).unwrap());
    let store = Store::open(&dir.path().join("registry.sqlite3"))
        .await
        .unwrap();
    let server = api::start(
        "127.0.0.1:0".parse().unwrap(),
        Arc::new(api::Context::new(store.clone(), OWNER, None, None).with_amazon(account)),
    )
    .unwrap();
    let base = format!("http://{}", server.local_addr());
    // Hold the real SQLite writer so disconnect reaches its scheduling commit.
    let mut tx = store.pool().begin().await.unwrap();
    sqlx::query("UPDATE amazon_discovery SET attempts=attempts WHERE id=1")
        .execute(&mut *tx)
        .await
        .unwrap();
    let disconnect = tokio::spawn(
        Client::new()
            .delete(format!("{base}/v1/amazon"))
            .bearer_auth(OWNER)
            .send(),
    );
    for _ in 0..100 {
        if !path.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        !path.exists(),
        "Disconnect did not reach the database boundary"
    );
    let mut begin = Box::pin(
        Client::new()
            .post(format!("{base}/v1/amazon/sign-in"))
            .bearer_auth(OWNER)
            .send(),
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(50), &mut begin)
            .await
            .is_err(),
        "A newer account change overtook the disconnect SQL commit"
    );
    tx.commit().await.unwrap();
    assert_eq!(
        disconnect.await.unwrap().unwrap().status(),
        StatusCode::NO_CONTENT
    );
    assert_eq!(begin.await.unwrap().status(), StatusCode::OK);
    server.close().await.unwrap();
    store.close().await.unwrap();
}
