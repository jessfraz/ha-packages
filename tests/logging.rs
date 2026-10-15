use std::{
    io::Write,
    sync::{Arc, Mutex},
};

use dropshot::{
    ApiDescription, ConfigDropshot, HttpError, HttpResponseOk, HttpServerStarter, Path,
    RequestContext, endpoint,
};
use reqwest::{Client, StatusCode};
use serde::Deserialize;
use shipments::{api, logging, store::Store};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use uuid::Uuid;

const OWNER: &str = "logging-test-owner-credential-at-least-32-bytes";

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

impl Capture {
    fn text(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
    }

    fn entries(&self) -> Vec<Entry> {
        self.text()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }
}

#[derive(Debug, Deserialize)]
struct Entry {
    timestamp: String,
    level: String,
    event: String,
    provider: Option<String>,
    request_id: Option<Uuid>,
    route: Option<String>,
    response_code: Option<u16>,
    error_code: Option<String>,
    operation: Option<String>,
    outcome: Option<String>,
    provider_status: Option<u16>,
    provider_error_code: Option<String>,
    retry_after_seconds: Option<u64>,
    duration_ms: Option<u64>,
}

#[derive(Deserialize)]
struct ErrorBody {
    request_id: Uuid,
}

#[tokio::test]
async fn http_logs_correlate_failures_and_mutations_without_private_request_data() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(&directory.path().join("logging.sqlite3"))
        .await
        .unwrap();
    let capture = Capture::default();
    let server = api::start_with_logger(
        "127.0.0.1:0".parse().unwrap(),
        Arc::new(api::Context::new(store.clone(), OWNER, None, None)),
        logging::with_writer(capture.clone()),
    )
    .unwrap();
    let base = format!("http://{}", server.local_addr());
    let client = Client::new();
    let responses = [
        (
            client
                .get(format!("{base}/v1/summary?key=QUERY_PRIVATE_SENTINEL"))
                .bearer_auth("AUTH_PRIVATE_SENTINEL")
                .send()
                .await
                .unwrap(),
            StatusCode::UNAUTHORIZED,
            "/v1/summary",
        ),
        (
            client
                .post(format!("{base}/v1/shipments?key=QUERY_PRIVATE_SENTINEL"))
                .bearer_auth(OWNER)
                .header("Content-Type", "application/json")
                .body("{\"title\":\"BODY_PRIVATE_SENTINEL")
                .send()
                .await
                .unwrap(),
            StatusCode::BAD_REQUEST,
            "/v1/shipments",
        ),
        (
            client
                .get(format!("{base}/v1/shipments/PATH_PRIVATE_SENTINEL/events"))
                .bearer_auth(OWNER)
                .send()
                .await
                .unwrap(),
            StatusCode::BAD_REQUEST,
            "/v1/shipments/{id}/events",
        ),
        (
            client
                .get(format!(
                    "{base}/UNMATCHED_PRIVATE_SENTINEL?key=QUERY_PRIVATE_SENTINEL"
                ))
                .bearer_auth(OWNER)
                .send()
                .await
                .unwrap(),
            StatusCode::NOT_FOUND,
            "unmatched",
        ),
    ];
    let mut expected = Vec::new();
    for (response, status, route) in responses {
        assert_eq!(response.status(), status);
        let request_id =
            Uuid::parse_str(response.headers()["x-request-id"].to_str().unwrap()).unwrap();
        let body: ErrorBody = response.json().await.unwrap();
        assert_eq!(body.request_id, request_id);
        expected.push((request_id, status.as_u16(), route));
    }
    let quiet = client
        .get(format!("{base}/v1/summary"))
        .bearer_auth(OWNER)
        .send()
        .await
        .unwrap();
    assert_eq!(quiet.status(), StatusCode::OK);
    let quiet_id = Uuid::parse_str(quiet.headers()["x-request-id"].to_str().unwrap()).unwrap();
    let created = client
        .post(format!("{base}/v1/agents"))
        .bearer_auth(OWNER)
        .header("Content-Type", "application/json")
        .body(r#"{"name":"AGENT_NAME_PRIVATE_SENTINEL"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(created.status(), StatusCode::CREATED);
    let created_id = Uuid::parse_str(created.headers()["x-request-id"].to_str().unwrap()).unwrap();
    expected.push((created_id, 201, "/v1/agents"));
    server.close().await.unwrap();
    store.close().await.unwrap();

    let entries = capture.entries();
    for entry in &entries {
        assert!(entry.timestamp.ends_with('Z'));
        OffsetDateTime::parse(&entry.timestamp, &Rfc3339).unwrap();
        assert!(!entry.level.is_empty());
    }
    assert!(entries.iter().any(|entry| entry.event == "server_started"));
    assert!(entries.iter().any(|entry| entry.event == "server_stopping"));
    assert!(
        !entries
            .iter()
            .any(|entry| entry.request_id == Some(quiet_id))
    );
    for (request_id, status, route) in expected {
        let entry = entries
            .iter()
            .find(|entry| entry.request_id == Some(request_id))
            .unwrap();
        assert_eq!(entry.event, "request_completed");
        assert_eq!(entry.response_code, Some(status));
        assert_eq!(entry.route.as_deref(), Some(route));
        assert_eq!(entry.error_code.is_some(), status >= 400);
        assert_eq!(entry.level, if status >= 400 { "warn" } else { "info" });
    }
    let output = capture.text();
    for private in [
        OWNER,
        "QUERY_PRIVATE_SENTINEL",
        "AUTH_PRIVATE_SENTINEL",
        "BODY_PRIVATE_SENTINEL",
        "PATH_PRIVATE_SENTINEL",
        "UNMATCHED_PRIVATE_SENTINEL",
        "AGENT_NAME_PRIVATE_SENTINEL",
        "127.0.0.1",
    ] {
        assert!(
            !output.contains(private),
            "private data appeared in logs: {private}"
        );
    }
}

#[derive(Deserialize, schemars::JsonSchema)]
struct RouteId {
    id: String,
}

#[endpoint {method=GET,path="/v1/shipments/{id}"}]
async fn private_error(
    _request: RequestContext<()>,
    path: Path<RouteId>,
) -> Result<HttpResponseOk<String>, HttpError> {
    let _ = path.into_inner().id;
    Err(HttpError {
        status_code: dropshot::ErrorStatusCode::INTERNAL_SERVER_ERROR,
        error_code: Some("PRIVATE_ERROR_CODE_SENTINEL".into()),
        external_message: "PRIVATE_EXTERNAL_ERROR_SENTINEL".into(),
        internal_message: "PRIVATE_INTERNAL_ERROR_SENTINEL".into(),
        headers: None,
    })
}

#[tokio::test]
async fn dropshot_error_text_is_replaced_with_a_safe_category() {
    let capture = Capture::default();
    let logger = logging::with_writer(capture.clone());
    let mut description = ApiDescription::new();
    description.register(private_error).unwrap();
    let server = HttpServerStarter::new(
        &ConfigDropshot {
            bind_address: "127.0.0.1:0".parse().unwrap(),
            ..Default::default()
        },
        description,
        (),
        &logger,
    )
    .unwrap()
    .start();
    let response = Client::new()
        .get(format!(
            "http://{}/v1/shipments/PRIVATE_PATH_SENTINEL",
            server.local_addr()
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let request_id = Uuid::parse_str(response.headers()["x-request-id"].to_str().unwrap()).unwrap();
    let body = response.text().await.unwrap();
    assert!(body.contains("PRIVATE_EXTERNAL_ERROR_SENTINEL"));
    server.close().await.unwrap();
    let entry = capture
        .entries()
        .into_iter()
        .find(|entry| entry.request_id == Some(request_id))
        .unwrap();
    assert_eq!(entry.route.as_deref(), Some("/v1/shipments/{id}"));
    assert_eq!(entry.error_code.as_deref(), Some("internal_error"));
    assert!(!capture.text().contains("PRIVATE_"));
}

#[test]
fn application_events_only_accept_typed_safe_fields_and_fixed_messages() {
    let capture = Capture::default();
    let logger = logging::with_writer(capture.clone());
    let request_id = Uuid::now_v7();
    slog::warn!(logger, "provider_request";
        "request_id" => request_id.to_string(), "operation" => "register", "outcome" => "rejected",
        "error_code" => "tracking_invalid", "provider_status" => Some(422_u16),
        "provider_error_code" => "TRACKER.INVALID_PARAMS",
        "retry_after_seconds" => Some(60_i64), "duration_ms" => 12_u128,
        "tracking_number" => "PRIVATE_TRACKING_SENTINEL", "api_key" => "PRIVATE_API_KEY_SENTINEL",
        "error_message_internal" => "PRIVATE_ERROR_SENTINEL",
    );
    slog::warn!(logger, "provider_request";
        "request_id" => "PRIVATE_REQUEST_ID_SENTINEL", "operation" => "PRIVATE_OPERATION_SENTINEL",
        "error_code" => "PRIVATE_ERROR_CODE_SENTINEL", "duration_ms" => u128::MAX,
        "provider_error_code" => "PRIVATE_PROVIDER_CODE_SENTINEL",
    );
    slog::warn!(
        logger,
        "PRIVATE_MESSAGE_SENTINEL {}",
        "PRIVATE_INTERPOLATED_SENTINEL"
    );
    let entries = capture.entries();
    assert_eq!(entries.len(), 2);
    let entry = &entries[0];
    assert_eq!(entry.event, "provider_request");
    assert_eq!(entry.request_id, Some(request_id));
    assert_eq!(entry.operation.as_deref(), Some("register"));
    assert_eq!(entry.outcome.as_deref(), Some("rejected"));
    assert_eq!(entry.error_code.as_deref(), Some("tracking_invalid"));
    assert_eq!(entry.provider_status, Some(422));
    assert_eq!(
        entry.provider_error_code.as_deref(),
        Some("TRACKER.INVALID_PARAMS")
    );
    assert_eq!(entry.retry_after_seconds, Some(60));
    assert_eq!(entry.duration_ms, Some(12));
    assert!(entries[1].request_id.is_none());
    assert!(entries[1].operation.is_none());
    assert!(entries[1].error_code.is_none());
    assert!(entries[1].provider_error_code.is_none());
    assert!(entries[1].duration_ms.is_none());
    assert!(!capture.text().contains("PRIVATE_"));
}

#[test]
fn recipient_and_aftership_failures_keep_safe_codes_without_recipient_links() {
    let capture = Capture::default();
    let logger = logging::with_writer(capture.clone());
    let recipient_url = "https://www.doordash.com/orders/drive?urlCode=PRIVATE_RECIPIENT_TOKEN";
    let request_id = Uuid::now_v7();
    let expected = [
        ("doordash", "tracking_recipient_url_required", None),
        ("doordash", "tracking_recipient_url_invalid", None),
        ("doordash", "tracking_recipient_url_mismatch", None),
        ("doordash", "recipient_page_unavailable", None),
        (
            "aftership",
            "tracking_carrier_credentials_required",
            Some("Pending_002"),
        ),
        (
            "aftership",
            "tracking_carrier_mismatch",
            Some("Pending_004"),
        ),
        ("aftership", "tracking_unsupported", Some("Pending_006")),
        ("aftership", "tracking_unsupported", Some("4010")),
        (
            "aftership",
            "tracking_carrier_credentials_required",
            Some("4011"),
        ),
        ("aftership", "tracking_unsupported", Some("4012")),
        ("aftership", "tracking_invalid", Some("4005")),
        ("aftership", "tracking_invalid", Some("4007")),
        ("aftership", "tracking_invalid", Some("4008")),
        ("aftership", "tracking_invalid", Some("4009")),
        ("aftership", "tracking_invalid", Some("4017")),
    ];
    for (provider, code, native_code) in expected {
        slog::warn!(logger, "provider_request";
            "provider" => provider, "request_id" => request_id.to_string(),
            "operation" => "retrieve", "outcome" => "rejected", "error_code" => code,
            "provider_status" => 400_u16, "provider_error_code" => native_code,
            "recipient_url" => recipient_url, "url" => recipient_url,
            "tracking_number" => "PRIVATE_TRACKING_NUMBER",
            "provider_response" => "PRIVATE_PROVIDER_RESPONSE",
        );
    }
    let entries = capture.entries();
    assert_eq!(entries.len(), expected.len());
    for (entry, (provider, code, native_code)) in entries.iter().zip(expected) {
        assert_eq!(entry.provider.as_deref(), Some(provider));
        assert_eq!(entry.request_id, Some(request_id));
        assert_eq!(entry.operation.as_deref(), Some("retrieve"));
        assert_eq!(entry.error_code.as_deref(), Some(code));
        assert_eq!(entry.provider_error_code.as_deref(), native_code);
        assert_eq!(entry.provider_status, Some(400));
    }
    let output = capture.text();
    assert!(!output.contains("PRIVATE_"));
    assert!(!output.contains("urlCode"));
    assert!(!output.contains("doordash.com"));
    assert!(!output.contains(recipient_url));
}
