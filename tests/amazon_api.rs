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

fn saved_account(path: &std::path::Path) {
    use std::os::unix::fs::OpenOptionsExt;

    #[derive(serde::Serialize)]
    struct SavedAccount {
        refresh_token: &'static str,
        device_serial: &'static str,
        customer_id: &'static str,
        generation: u64,
        imports_enabled: bool,
    }
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .unwrap();
    serde_json::to_writer(
        file,
        &SavedAccount {
            refresh_token: "SYNTHETIC_REFRESH",
            device_serial: "SYNTHETIC_SERIAL",
            customer_id: "SYNTHETIC_CUSTOMER",
            generation: 1,
            imports_enabled: false,
        },
    )
    .unwrap();
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
        (
            Method::POST,
            "/v1/shipments/01930000-0000-7000-8000-000000000001/amazon",
        ),
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
    assert_eq!(response.headers()["cache-control"], "no-store");
    let failure: dropshot::HttpErrorResponseBody = response.json().await.unwrap();
    assert_eq!(
        failure.error_code.as_deref(),
        Some("amazon_connect_callback_invalid_input")
    );
    assert!(uuid::Uuid::parse_str(&failure.request_id).is_ok());
    assert!(!failure.message.contains("PRIVATE-AUTHORIZATION-CODE"));
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
    #[derive(Deserialize)]
    struct OperationLog {
        event: String,
        operation: Option<String>,
        error_code: Option<String>,
    }
    let entries: Vec<OperationLog> = log
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert!(
        entries
            .iter()
            .any(|entry| entry.event == "amazon_connection"
                && entry.operation.as_deref() == Some("callback")
                && entry.error_code.as_deref() == Some("invalid_input"))
    );
    assert!(
        entries
            .iter()
            .any(|entry| entry.event == "amazon_connection"
                && entry.operation.as_deref() == Some("flow")
                && entry.error_code.as_deref() == Some("expired_flow"))
    );
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
async fn existing_amazon_adoption_checks_review_and_eligibility_before_upstream() {
    use shipments::{
        carrier::Carrier,
        models::{CarrierTracking, CreateShipment, Direction, InputTracking, Status},
        shipping_api::types::{ProviderId, Tracker},
    };
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::AsyncWriteExt;

    let directory = tempfile::tempdir().unwrap();
    let credential_path = directory.path().join("amazon.json");
    saved_account(&credential_path);
    let credential_before = std::fs::read(&credential_path).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint =
        reqwest::Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
    let upstream_calls = Arc::new(AtomicUsize::new(0));
    let calls = upstream_calls.clone();
    let upstream = tokio::spawn(async move {
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            calls.fetch_add(1, Ordering::SeqCst);
            stream
                .write_all(
                    b"HTTP/1.1 503 fixture\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await
                .unwrap();
        }
    });
    let amazon = Arc::new(AmazonClient::for_test(credential_path.clone(), endpoint).unwrap());
    let store = Store::open(&directory.path().join("registry.sqlite3"))
        .await
        .unwrap();
    let mut records = Vec::new();
    for (carrier, number) in [
        (Carrier::AmazonMcf, "TBA000000000031"),
        (Carrier::Ups, "1Z000000000000032"),
    ] {
        let verified = Tracker {
            provider: ProviderId::for_carrier(Some(carrier)),
            id: "synthetic-original-tracker".into(),
            tracking_code: number.into(),
            carrier,
            status: Status::Unknown,
            public_url: None,
            carrier_tracking: CarrierTracking::default(),
        };
        records.push(
            store
                .create(
                    CreateShipment {
                        title: "Owner's existing shipment".into(),
                        direction: Direction::Inbound,
                        merchant: Some("Amazon".into()),
                        order_reference: Some("111-0000000-0000001".into()),
                        signature_required: None,
                        tracking: InputTracking {
                            carrier: Some(carrier),
                            number: number.into(),
                            recipient_url: None,
                        },
                        source: None,
                    },
                    "owner",
                    number,
                    Some(&verified),
                )
                .await
                .unwrap(),
        );
    }
    let server = api::start(
        "127.0.0.1:0".parse().unwrap(),
        Arc::new(api::Context::new(store.clone(), OWNER, None, None).with_amazon(amazon.clone())),
    )
    .unwrap();
    let base = format!("http://{}", server.local_addr());
    let client = Client::new();
    for (record, revision, status) in [
        (&records[0], None, StatusCode::PRECONDITION_REQUIRED),
        (
            &records[0],
            Some(records[0].revision + 1),
            StatusCode::PRECONDITION_FAILED,
        ),
        (&records[1], Some(records[1].revision), StatusCode::CONFLICT),
    ] {
        let mut request = client
            .post(format!("{base}/v1/shipments/{}/amazon", record.id))
            .bearer_auth(OWNER);
        if let Some(revision) = revision {
            request = request.header("If-Match", format!("\"{revision}\""));
        }
        assert_eq!(request.send().await.unwrap().status(), status);
        assert_eq!(
            serde_json::to_string(&store.get(record.id).await.unwrap()).unwrap(),
            serde_json::to_string(record).unwrap()
        );
    }
    assert_eq!(upstream_calls.load(Ordering::SeqCst), 0);
    assert!(!amazon.status().await.imports_enabled);
    assert_eq!(std::fs::read(&credential_path).unwrap(), credential_before);
    let mappings: i64 = sqlx::query_scalar("SELECT count(*) FROM amazon_parcels")
        .fetch_one(store.pool())
        .await
        .unwrap();
    assert_eq!(mappings, 0);
    upstream.abort();
    server.close().await.unwrap();
    store.close().await.unwrap();
}

#[tokio::test]
async fn existing_amazon_adoption_verifies_upstream_and_commits_the_same_record() {
    use shipments::{
        carrier::Carrier,
        models::{
            CarrierEvent, CarrierTracking, CreateShipment, Direction, InputTracking, Shipment,
            Source, Status,
        },
        shipping_api::types::{ProviderId, Tracker},
    };
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

    const NUMBER: &str = "TBA000000000041";
    const ORDER: &str = "111-1234567-1234567";
    const DETAILS: &str = "/your-orders/order-details?orderID=111-1234567-1234567";
    const TRACK: &str =
        "/progress-tracker/package?orderId=111-1234567-1234567&shipmentId=SHIP1&packageIndex=0";
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint =
        reqwest::Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
    let upstream = tokio::spawn(async move {
        let mut paths = Vec::new();
        for _ in 0..4 {
            let (mut stream, _) = listener.accept().await.unwrap();
            let path = {
                let mut reader = BufReader::new(&mut stream);
                let mut line = String::new();
                reader.read_line(&mut line).await.unwrap();
                let path = line.split_whitespace().nth(1).unwrap().to_owned();
                let mut length = 0;
                loop {
                    line.clear();
                    assert!(reader.read_line(&mut line).await.unwrap() > 0);
                    if line == "\r\n" {
                        break;
                    }
                    if let Some((name, value)) = line.split_once(':')
                        && name.eq_ignore_ascii_case("content-length")
                    {
                        length = value.trim().parse::<usize>().unwrap();
                    }
                }
                assert!(length < 16 * 1024);
                reader.read_exact(&mut vec![0; length]).await.unwrap();
                path
            };
            let body = match path.as_str() {
                "/auth/token" => r#"{"access_token":"SYNTHETIC_ACCESS"}"#.to_owned(),
                "/ap/exchangetoken/cookies" => r#"{"response":{"tokens":{"cookies":{".amazon.com":[{"Name":"session-id","Value":"SYNTHETIC_COOKIE","Path":"/"}]}}}}"#.to_owned(),
                DETAILS => format!(r#"<a href="{TRACK}">Track package</a>"#),
                TRACK => format!(r#"<h3>Delivery facilitated by Amazon</h3><script id="page-state">{{"trackingId":"{NUMBER}","orderId":"{ORDER}","shipmentId":"SHIP1","packageIndex":"0","shortStatus":"DELIVERED","progressTracker":{{"lastReachedMilestone":"DELIVERED"}}}}</script>"#),
                other => panic!("Unexpected synthetic upstream path: {other}"),
            };
            let response = format!(
                "HTTP/1.1 200 fixture\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).await.unwrap();
            paths.push(path);
        }
        paths
    });
    let directory = tempfile::tempdir().unwrap();
    let credential_path = directory.path().join("amazon.json");
    saved_account(&credential_path);
    let amazon = Arc::new(AmazonClient::for_test(credential_path, endpoint).unwrap());
    let store = Store::open(&directory.path().join("registry.sqlite3"))
        .await
        .unwrap();
    let previous = Tracker {
        provider: ProviderId::AfterShip,
        id: "synthetic-original-tracker".into(),
        tracking_code: NUMBER.into(),
        carrier: Carrier::AmazonMcf,
        status: Status::Unknown,
        public_url: None,
        carrier_tracking: CarrierTracking {
            events: vec![CarrierEvent {
                status: Some(Status::InTransit),
                datetime: Some("2026-10-01T12:00:00Z".into()),
                message: Some("Existing provider scan".into()),
                ..Default::default()
            }],
            ..Default::default()
        },
    };
    let before = store
        .create(
            CreateShipment {
                title: "Owner's existing parcel title".into(),
                direction: Direction::Inbound,
                merchant: Some("Amazon".into()),
                order_reference: Some(ORDER.into()),
                signature_required: Some(true),
                tracking: InputTracking {
                    carrier: Some(Carrier::AmazonMcf),
                    number: NUMBER.into(),
                    recipient_url: None,
                },
                source: Some(Source {
                    namespace: "fixture-mail".into(),
                    key: "synthetic-source-message".into(),
                }),
            },
            "owner",
            NUMBER,
            Some(&previous),
        )
        .await
        .unwrap();
    let before_events = store.events(before.id, 0, 100).await.unwrap();
    let server = api::start_with_logger(
        "127.0.0.1:0".parse().unwrap(),
        Arc::new(api::Context::new(store.clone(), OWNER, None, None).with_amazon(amazon.clone())),
        logging::with_writer(Capture::default()),
    )
    .unwrap();
    let response = Client::new()
        .post(format!(
            "http://{}/v1/shipments/{}/amazon",
            server.local_addr(),
            before.id
        ))
        .bearer_auth(OWNER)
        .header("If-Match", format!("\"{}\"", before.revision))
        .send()
        .await
        .unwrap();
    if response.status() != StatusCode::OK {
        let status = response.status();
        let error: dropshot::HttpErrorResponseBody = response.json().await.unwrap();
        panic!("Unexpected adoption response {status}: {error:?}");
    }
    let etag = response.headers()["etag"].to_str().unwrap().to_owned();
    let adopted: Shipment = response.json().await.unwrap();
    assert_eq!(adopted.id, before.id);
    assert_eq!(adopted.tracking.carrier.as_str(), "AmazonAccount");
    assert_eq!(adopted.tracking.number, before.tracking.number);
    assert_eq!(
        adopted.tracking.recipient_url,
        before.tracking.recipient_url
    );
    assert_eq!(adopted.title, before.title);
    assert_eq!(adopted.created_at, before.created_at);
    assert!(adopted.revision > before.revision);
    assert_eq!(etag, format!("\"{}\"", adopted.revision));
    assert_eq!(adopted.sync.provider, ProviderId::AmazonAccount);
    assert_eq!(adopted.status, Status::Delivered);
    assert_eq!(adopted.carrier_tracking, before.carrier_tracking);
    assert!(before.sources.iter().all(|s| adopted.sources.contains(s)));
    assert!(
        adopted
            .sources
            .iter()
            .any(|s| s.namespace == "amazon-account")
    );
    let after_events = store.events(before.id, 0, 100).await.unwrap();
    assert_eq!(after_events.len(), before_events.len() + 1);
    assert_eq!(
        serde_json::to_string(&after_events[..before_events.len()]).unwrap(),
        serde_json::to_string(&before_events).unwrap()
    );
    assert_eq!(
        serde_json::to_string(&store.get(before.id).await.unwrap()).unwrap(),
        serde_json::to_string(&adopted).unwrap()
    );
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM shipments")
        .fetch_one(store.pool())
        .await
        .unwrap();
    assert_eq!(count, 1);
    assert!(!amazon.status().await.imports_enabled);
    assert_eq!(
        upstream.await.unwrap(),
        ["/auth/token", "/ap/exchangetoken/cookies", DETAILS, TRACK]
    );
    server.close().await.unwrap();
    store.close().await.unwrap();
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

#[tokio::test]
async fn every_downstream_connection_boundary_has_a_safe_correlated_error_and_log() {
    use std::os::unix::fs::OpenOptionsExt;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    #[derive(Clone, Copy)]
    struct Case {
        failed_path: &'static str,
        upstream_status: u16,
        body: &'static str,
        stage: &'static str,
        reason: &'static str,
        response_status: StatusCode,
        local_failure: bool,
    }
    const REGISTER: &str = r#"{"response":{"success":{"tokens":{"bearer":{"refresh_token":"PRIVATE_REFRESH_SENTINEL"}},"extensions":{"customer_info":{"user_id":"SYNTHETIC_CUSTOMER"}}}}}"#;
    let cases = [
        Case {
            failed_path: "/auth/register",
            upstream_status: 200,
            body: r#"{"response":{"success":{"tokens":{"bearer":{"refresh_token":"PRIVATE_REFRESH_SENTINEL"}}}}}"#,
            stage: "registration",
            reason: "account_identity_missing",
            response_status: StatusCode::BAD_GATEWAY,
            local_failure: false,
        },
        Case {
            failed_path: "/auth/register",
            upstream_status: 401,
            body: r#"{"error":"invalid_grant","message":"PRIVATE_PROVIDER_TEXT"}"#,
            stage: "registration",
            reason: "upstream_rejected",
            response_status: StatusCode::BAD_REQUEST,
            local_failure: false,
        },
        Case {
            failed_path: "/auth/token",
            upstream_status: 200,
            body: r#"{"unexpected":"PRIVATE_ACCESS_SENTINEL"}"#,
            stage: "access_token",
            reason: "unexpected_response",
            response_status: StatusCode::BAD_GATEWAY,
            local_failure: false,
        },
        Case {
            failed_path: "/ap/exchangetoken/cookies",
            upstream_status: 200,
            body: r#"{"unexpected":"PRIVATE_COOKIE_SENTINEL"}"#,
            stage: "cookies",
            reason: "unexpected_response",
            response_status: StatusCode::BAD_GATEWAY,
            local_failure: false,
        },
        Case {
            failed_path: "/your-orders/orders?timeFilter=months-3",
            upstream_status: 200,
            body: "<html>PRIVATE_ORDER_SENTINEL</html>",
            stage: "orders",
            reason: "unexpected_response",
            response_status: StatusCode::BAD_GATEWAY,
            local_failure: false,
        },
        Case {
            failed_path: "/auth/register",
            upstream_status: 200,
            body: REGISTER,
            stage: "account",
            reason: "account_mismatch",
            response_status: StatusCode::CONFLICT,
            local_failure: true,
        },
        Case {
            failed_path: "/your-orders/orders?timeFilter=months-3",
            upstream_status: 200,
            body: r#"<a class="item-card__link" aria-label="Fixture item, Order placed 1 October 2026, Shipped" href="/gp/your-account/ship-track?orderId=111-1234567-1234567&amp;shipmentId=SHIP1&amp;packageId=1"></a>"#,
            stage: "credentials",
            reason: "persistence",
            response_status: StatusCode::SERVICE_UNAVAILABLE,
            local_failure: true,
        },
    ];
    for case in cases {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint =
            reqwest::Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
        let upstream = tokio::spawn(async move {
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                let mut buffer = [0; 4096];
                let path = loop {
                    let n = stream.read(&mut buffer).await.unwrap();
                    assert!(n > 0);
                    request.extend_from_slice(&buffer[..n]);
                    if let Some(end) = request.windows(4).position(|v| v == b"\r\n\r\n") {
                        let headers = std::str::from_utf8(&request[..end]).unwrap();
                        let length: usize = headers
                            .lines()
                            .find_map(|line| {
                                line.to_ascii_lowercase()
                                    .strip_prefix("content-length: ")
                                    .map(str::to_owned)
                            })
                            .map(|v| v.parse().unwrap())
                            .unwrap_or(0);
                        if request.len() >= end + 4 + length {
                            break headers
                                .lines()
                                .next()
                                .unwrap()
                                .split_whitespace()
                                .nth(1)
                                .unwrap()
                                .to_owned();
                        }
                    }
                };
                let (status, body) = if path == case.failed_path
                    || (case.stage == "orders"
                        && path == "/gp/css/order-history?timeFilter=months-3")
                {
                    (case.upstream_status, case.body)
                } else {
                    (
                        200,
                        match path.as_str() {
                            "/auth/register" => REGISTER,
                            "/auth/token" => r#"{"access_token":"PRIVATE_ACCESS_SENTINEL"}"#,
                            "/ap/exchangetoken/cookies" => {
                                r#"{"response":{"tokens":{"cookies":{".amazon.com":[{"Name":"session-id","Value":"PRIVATE_COOKIE_SENTINEL","Path":"/"}]}}}}"#
                            }
                            _ => panic!("Unexpected synthetic upstream path"),
                        },
                    )
                };
                let reply = format!(
                    "HTTP/1.1 {status} fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(reply.as_bytes()).await.unwrap();
                if (path == case.failed_path && case.stage != "orders")
                    || (case.stage == "orders"
                        && path == "/gp/css/order-history?timeFilter=months-3")
                    || (case.stage == "credentials" && path == "/auth/token")
                {
                    break;
                }
            }
        });
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("amazon.json");
        if case.stage == "account" {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&path)
                .unwrap();
            file.write_all(br#"{"refresh_token":"PRIVATE_EXISTING_SENTINEL","device_serial":"SYNTHETIC_SERIAL","customer_id":"EXISTING_CUSTOMER","generation":1,"imports_enabled":false}"#).unwrap();
        }
        let original = std::fs::read(&path).ok();
        let account = Arc::new(AmazonClient::for_test(path.clone(), endpoint).unwrap());
        let store = Store::open(&dir.path().join("registry.sqlite3"))
            .await
            .unwrap();
        let capture = Capture::default();
        let server = api::start_with_logger(
            "127.0.0.1:0".parse().unwrap(),
            Arc::new(
                api::Context::new(store.clone(), OWNER, None, None).with_amazon(account.clone()),
            ),
            logging::with_writer(capture.clone()),
        )
        .unwrap();
        let client = Client::new();
        let base = format!("http://{}", server.local_addr());
        let flow: SignIn = client
            .post(format!("{base}/v1/amazon/sign-in"))
            .bearer_auth(OWNER)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        if case.stage == "credentials" {
            std::fs::create_dir(&path).unwrap();
        }
        #[derive(serde::Serialize)]
        struct Complete<'a> {
            flow_id: &'a str,
            landing_url: &'a str,
        }
        let response=client.post(format!("{base}/v1/amazon/complete")).bearer_auth(OWNER).json(&Complete{flow_id:&flow.flow_id,landing_url:"https://www.amazon.com/ap/maplanding?openid.oa2.authorization_code=PRIVATE_CODE_SENTINEL"}).send().await.unwrap();
        let preview = matches!(case.stage, "cookies" | "orders");
        let response = if preview {
            assert_eq!(response.status(), StatusCode::OK);
            client
                .post(format!("{base}/v1/amazon/preview"))
                .bearer_auth(OWNER)
                .send()
                .await
                .unwrap()
        } else {
            response
        };
        assert_eq!(response.status(), case.response_status);
        assert_eq!(response.headers()["cache-control"], "no-store");
        let failure: dropshot::HttpErrorResponseBody = response.json().await.unwrap();
        assert_eq!(
            failure.error_code,
            Some(format!(
                "amazon_{}_{}_{}",
                if preview { "preview" } else { "connect" },
                case.stage,
                if case.stage == "orders" {
                    "unsupported_orders_layout"
                } else {
                    case.reason
                }
            ))
        );
        assert!(uuid::Uuid::parse_str(&failure.request_id).is_ok());
        assert!(!failure.message.contains("PRIVATE_"));
        if case.stage == "account" {
            assert!(
                std::fs::read(&path).ok() == original,
                "Existing credential file changed"
            );
        } else if case.stage == "credentials" {
            assert!(path.is_dir());
        } else if preview {
            assert!(path.exists());
            assert_eq!(
                account.status().await.state,
                shipments::amazon::AmazonState::Connected
            );
        } else {
            assert!(!path.exists());
        }
        assert!(!account.status().await.imports_enabled);
        upstream.await.unwrap();
        server.close().await.unwrap();
        store.close().await.unwrap();
        #[derive(Deserialize)]
        struct Entry {
            event: String,
            request_id: Option<String>,
            operation: Option<String>,
            error_code: Option<String>,
            provider_status: Option<u16>,
            duration_ms: Option<u64>,
        }
        let log = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
        let entries: Vec<Entry> = log
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let event = entries
            .iter()
            .find(|e| e.event == "amazon_connection")
            .unwrap();
        assert_eq!(
            event.request_id.as_deref(),
            Some(failure.request_id.as_str())
        );
        assert_eq!(event.operation.as_deref(), Some(case.stage));
        assert_eq!(
            event.error_code.as_deref(),
            Some(if case.stage == "orders" {
                "unsupported_orders_layout"
            } else {
                case.reason
            })
        );
        assert_eq!(
            event.provider_status,
            (!case.local_failure).then_some(case.upstream_status)
        );
        assert!(event.duration_ms.is_some());
        assert!(!log.contains("PRIVATE_"));
        assert!(!log.contains(&flow.flow_id));
    }
}
