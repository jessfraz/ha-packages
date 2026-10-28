//! Real process and wire tests. No provider credentials or live network needed.
use serde::{Deserialize, Serialize};
use shipments::{carrier::Carrier, models::*};
use std::{
    collections::HashMap,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    process::{Command, Output, Stdio},
    thread,
    time::{Duration, Instant},
};
use tempfile::NamedTempFile;
use uuid::Uuid;

const ID: &str = "00000000-0000-4000-8000-000000000001";
const SECRET: &str = "fixture-only-secret";

fn command(url: &str, ha: bool) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_ha-shipments"));
    for name in [
        "SHIPMENTS_URL",
        "SHIPMENTS_API_TOKEN",
        "SHIPMENTS_AGENT_TOKEN",
        "HOME_ASSISTANT_URL",
        "HOME_ASSISTANT_TOKEN",
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
        "http_proxy",
        "https_proxy",
        "all_proxy",
    ] {
        command.env_remove(name);
    }
    command
        .env(
            if ha {
                "HOME_ASSISTANT_URL"
            } else {
                "SHIPMENTS_URL"
            },
            url,
        )
        .env(
            if ha {
                "HOME_ASSISTANT_TOKEN"
            } else {
                "SHIPMENTS_AGENT_TOKEN"
            },
            SECRET,
        )
        .env("NO_PROXY", "*");
    command
}

fn invoke(mut command: Command, args: &[&str], input: Option<&str>) -> Output {
    command
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().unwrap();
    if let Some(input) = input {
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
    }
    drop(child.stdin.take());
    child.wait_with_output().unwrap()
}

struct Request {
    method: String,
    path: String,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}
fn request(mut stream: &TcpStream) -> Request {
    stream.set_nonblocking(false).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut bytes = Vec::new();
    let header_end = loop {
        let mut b = [0];
        stream.read_exact(&mut b).unwrap();
        bytes.push(b[0]);
        if bytes.ends_with(b"\r\n\r\n") {
            break bytes.len();
        }
        assert!(bytes.len() < 16384);
    };
    let text = String::from_utf8(bytes.clone()).unwrap();
    let mut lines = text.split("\r\n");
    let mut first = lines.next().unwrap().split_whitespace();
    let method = first.next().unwrap().into();
    let path = first.next().unwrap().into();
    let headers: HashMap<String, String> = lines
        .filter_map(|line| line.split_once(": "))
        .map(|(k, v)| (k.to_ascii_lowercase(), v.into()))
        .collect();
    let length: usize = headers
        .get("content-length")
        .map(|v| v.parse().unwrap())
        .unwrap_or(0);
    bytes.resize(header_end + length, 0);
    stream.read_exact(&mut bytes[header_end..]).unwrap();
    Request {
        method,
        path,
        headers,
        body: bytes[header_end..].to_vec(),
    }
}

fn fixture(status: u16, headers: &str, body: String) -> (String, thread::JoinHandle<Request>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    listener.set_nonblocking(true).unwrap();
    let headers = headers.to_owned();
    let handle = thread::spawn(move || {
        let start = Instant::now();
        let mut stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(
                        start.elapsed() < Duration::from_secs(10),
                        "CLI made no request"
                    );
                    thread::sleep(Duration::from_millis(5));
                }
                Err(e) => panic!("{e}"),
            }
        };
        let request = request(&stream);
        write!(stream, "HTTP/1.1 {status} Fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n{headers}\r\n{body}", body.len()).unwrap();
        request
    });
    (url, handle)
}

#[derive(Deserialize)]
struct Envelope<T> {
    status: u16,
    etag: Option<String>,
    data: T,
}
#[derive(Deserialize, Serialize)]
struct Page<T> {
    items: Vec<T>,
    next_page: Option<String>,
}

fn success<T: for<'de> Deserialize<'de>>(output: &Output) -> Envelope<T> {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    serde_json::from_slice(&output.stdout).unwrap()
}

fn failure(output: &Output, expected: &str) {
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains(expected), "{stderr}");
    for private in [
        SECRET,
        "sensitive.example",
        "sensitive-body",
        "private-file",
    ] {
        assert!(!stderr.contains(private), "{stderr}");
    }
}

fn shipment() -> Shipment {
    // Construct via the real owned model so the fixture tracks contract changes.
    Shipment {
        id: Uuid::parse_str(ID).unwrap(),
        revision: 7,
        title: "Fixture parcel".into(),
        direction: Direction::Inbound,
        merchant: None,
        order_reference: None,
        signature_required: None,
        tracking: Tracking {
            carrier: Carrier::Usps,
            number: "TEST000001".into(),
            recipient_url: None,
        },
        status: Status::InTransit,
        carrier_status: Some(Status::InTransit),
        carrier_tracking: CarrierTracking::default(),
        status_override: None,
        archived: false,
        created_at: "2030-01-01T00:00:00Z".into(),
        updated_at: "2030-01-01T00:00:00Z".into(),
        sources: vec![],
        sync: SyncState {
            provider: Default::default(),
            registration_state: RegistrationState::Registered,
            registration_started_at: None,
            error_code: None,
            provider_status: None,
            provider_error_code: None,
            tracker_id: None,
            tracking_url: None,
            last_success_at: None,
            last_error: None,
            next_attempt_at: None,
            attempts: 0,
        },
    }
}

#[test]
fn list_is_one_page_and_preserves_cursor_and_transport_auth() {
    for ha in [false, true] {
        let page = Page {
            items: vec![shipment()],
            next_page: Some("cursor+/=?".into()),
        };
        let (url, handle) = fixture(200, "", serde_json::to_string(&page).unwrap());
        let output = invoke(
            command(&url, ha),
            &["list", "--limit", "2", "--page-token", "cursor+/=?"],
            None,
        );
        let envelope: Envelope<Page<Shipment>> = success(&output);
        assert_eq!(envelope.status, 200);
        assert_eq!(envelope.data.next_page, page.next_page);
        assert_eq!(envelope.data.items[0].id, shipment().id);
        let request = handle.join().unwrap();
        assert_eq!(request.method, "GET");
        let parsed = reqwest::Url::parse(&format!("{url}{}", request.path)).unwrap();
        assert_eq!(
            parsed.path(),
            if ha {
                "/api/shipments/v1/shipments"
            } else {
                "/v1/shipments"
            }
        );
        let query: HashMap<_, _> = parsed.query_pairs().into_owned().collect();
        assert_eq!(query.get("page_token").unwrap(), "cursor+/=?");
        assert_eq!(query.get("limit").unwrap(), "2");
        assert_eq!(request.headers["authorization"], format!("Bearer {SECRET}"));
        assert!(!request.headers.contains_key("idempotency-key"));
    }
}

#[test]
fn filters_and_summary_use_typed_json_and_encoded_query() {
    let mut file = NamedTempFile::new().unwrap();
    write!(file, "{{\"search\":\"a & b\",\"active\":false,\"direction\":\"outbound\",\"tracking_issues\":true,\"sort\":\"delivered\"}}").unwrap();
    let (url, handle) = fixture(
        200,
        "",
        serde_json::to_string(&Page::<Shipment> {
            items: vec![],
            next_page: None,
        })
        .unwrap(),
    );
    success::<Page<Shipment>>(&invoke(
        command(&url, false),
        &["list", "--input", file.path().to_str().unwrap()],
        None,
    ));
    let request = handle.join().unwrap();
    let url = reqwest::Url::parse(&format!("{url}{}", request.path)).unwrap();
    let query: HashMap<_, _> = url.query_pairs().into_owned().collect();
    assert_eq!(query.get("search").unwrap(), "a & b");
    assert_eq!(query.get("active").unwrap(), "false");
    assert_eq!(query.get("sort").unwrap(), "delivered");

    let (url, handle) = fixture(200, "", serde_json::to_string(&Summary::default()).unwrap());
    success::<Summary>(&invoke(
        command(&url, true),
        &["summary", "--input", "-"],
        Some(r#"{"direction":"inbound","delivered_after":10,"delivered_before":20}"#),
    ));
    assert_eq!(
        handle.join().unwrap().path,
        "/api/shipments/v1/summary?direction=inbound&delivered_after=10&delivered_before=20"
    );
}

#[test]
fn create_preserves_explicit_key_and_carrier_input_alias() {
    let (url, handle) = fixture(
        201,
        "ETag: \"7\"\r\n",
        serde_json::to_string(&shipment()).unwrap(),
    );
    let input = r#"{"title":"Fixture parcel","direction":"inbound","tracking":{"number":"TEST000001","carrier":"FedExAccount"}}"#;
    let output = invoke(
        command(&url, false),
        &["create", "--idempotency-key", "fixture-stable-key"],
        Some(input),
    );
    let envelope: Envelope<Shipment> = success(&output);
    assert_eq!(envelope.status, 201);
    assert_eq!(envelope.etag.as_deref(), Some("\"7\""));
    let request = handle.join().unwrap();
    assert_eq!(request.method, "POST");
    assert_eq!(request.path, "/v1/shipments");
    assert_eq!(request.headers["idempotency-key"], "fixture-stable-key");
    assert_eq!(request.headers["content-type"], "application/json");
    let input: CreateShipment = serde_json::from_slice(&request.body).unwrap();
    assert_eq!(input.tracking.carrier, Some(Carrier::FedEx));
    assert!(!request.headers.contains_key("if-match"));
}

#[test]
fn patch_preserves_null_vs_missing_and_exact_etag() {
    let (url, handle) = fixture(
        200,
        "ETag: \"7\"\r\n",
        serde_json::to_string(&shipment()).unwrap(),
    );
    success::<Shipment>(&invoke(
        command(&url, true),
        &["patch", ID, "--if-match", "\"6\""],
        Some(r#"{"merchant":null,"signature_required":false}"#),
    ));
    let request = handle.join().unwrap();
    assert_eq!(request.method, "PATCH");
    assert_eq!(request.headers["if-match"], "\"6\"");
    let patch: UpdateShipment = serde_json::from_slice(&request.body).unwrap();
    assert_eq!(patch.merchant.value, Some(None));
    assert_eq!(patch.signature_required.value, Some(Some(false)));
    assert!(patch.tracking.is_missing());
    assert!(patch.order_reference.is_missing());
    assert!(!request.headers.contains_key("idempotency-key"));
}

#[test]
fn existing_operations_use_documented_methods_paths_and_conditions() {
    for (args, method, suffix, input) in [
        (
            "add-source",
            "POST",
            "sources",
            Some(r#"{"namespace":"fixture","key":"one"}"#),
        ),
        (
            "set-override",
            "PUT",
            "status-override",
            Some(r#"{"status":"delivered","reason":"fixture correction"}"#),
        ),
        ("clear-override", "DELETE", "status-override", None),
        ("archive", "PUT", "archive", None),
        ("restore", "DELETE", "archive", None),
        ("refresh", "POST", "refresh", None),
    ] {
        let (url, handle) = fixture(
            200,
            "ETag: \"7\"\r\n",
            serde_json::to_string(&shipment()).unwrap(),
        );
        success::<Shipment>(&invoke(
            command(&url, false),
            &[args, ID, "--if-match", "\"6\""],
            input,
        ));
        let request = handle.join().unwrap();
        assert_eq!(request.method, method);
        assert_eq!(request.path, format!("/v1/shipments/{ID}/{suffix}"));
        assert_eq!(request.headers["if-match"], "\"6\"");
        if input.is_none() {
            assert!(request.body.is_empty());
        }
    }
}

#[test]
fn errors_and_redirects_never_echo_sensitive_response_or_location() {
    let sink = TcpListener::bind("127.0.0.1:0").unwrap();
    sink.set_nonblocking(true).unwrap();
    for status in [
        301, 302, 303, 307, 308, 400, 401, 403, 409, 412, 428, 429, 500, 503,
    ] {
        let headers = format!(
            "Location: http://{}/sensitive.example?token={SECRET}\r\nRetry-After: 30\r\n",
            sink.local_addr().unwrap()
        );
        let (url, handle) = fixture(
            status,
            &headers,
            format!("sensitive-body {SECRET} https://sensitive.example"),
        );
        let output = invoke(command(&url, false), &["get", ID], None);
        failure(
            &output,
            if status < 400 {
                "redirect_refused"
            } else {
                "http_error"
            },
        );
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(stderr.contains(&format!("\"status\":{status}")));
        assert!(stderr.contains("\"retry_after_seconds\":30"));
        handle.join().unwrap();
        assert!(matches!(sink.accept(), Err(e) if e.kind() == std::io::ErrorKind::WouldBlock));
    }
}

#[test]
fn invalid_success_and_connection_errors_are_private() {
    let (url, handle) = fixture(200, "", format!("{{\"sensitive-body\":\"{SECRET}\"}}"));
    failure(
        &invoke(command(&url, false), &["get", ID], None),
        "invalid_json_response_outcome_unknown",
    );
    handle.join().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);
    failure(
        &invoke(command(&url, false), &["get", ID], None),
        "request_failed_outcome_unknown",
    );
}

#[test]
fn invalid_inputs_and_auth_selection_fail_before_any_request() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    for input in [
        r#"{"title":"sensitive-body","direction":"sideways","tracking":{"number":"TEST000001"}}"#,
        r#"{"title":"sensitive-body","direction":"inbound","tracking":{"number":"TEST000001"},"actor":"owner"}"#,
        "not json sensitive-body",
    ] {
        failure(
            &invoke(
                command(&url, false),
                &["create", "--idempotency-key", "one"],
                Some(input),
            ),
            "invalid_json_input",
        );
    }
    for etag in ["6", "*", "W/\"6\"", "\"0\"", "\"-1\"", "\"1\",\"2\""] {
        failure(
            &invoke(
                command(&url, false),
                &["refresh", ID, "--if-match", etag],
                None,
            ),
            "if_match_must_be_quoted_positive_revision",
        );
    }
    failure(
        &invoke(command(&url, false), &["create"], None),
        "invalid_arguments",
    );
    failure(
        &invoke(command(&url, false), &["list", "--limit", "201"], None),
        "invalid_arguments",
    );
    failure(
        &invoke(
            command(&url, false),
            &["list", "--input", "/private-file"],
            None,
        ),
        "input_open_failed",
    );
    for bad in [
        "https://user:secret@sensitive.example",
        "https://sensitive.example/path",
        "https://sensitive.example/?token=secret",
        "https://sensitive.example/#secret",
        "file:///private-file",
    ] {
        failure(
            &invoke(command(bad, false), &["system"], None),
            "url_must_be_http_origin",
        );
    }
    let mut mixed = command(&url, false);
    mixed
        .env("HOME_ASSISTANT_URL", &url)
        .env("HOME_ASSISTANT_TOKEN", SECRET);
    failure(&invoke(mixed, &["system"], None), "select_transport");
    let mut missing = command(&url, false);
    missing
        .env_remove("SHIPMENTS_AGENT_TOKEN")
        .env("HOME_ASSISTANT_TOKEN", SECRET);
    failure(&invoke(missing, &["system"], None), "missing_token");
    let mut double = command(&url, false);
    double.env("SHIPMENTS_API_TOKEN", SECRET);
    failure(
        &invoke(double, &["system"], None),
        "select_one_native_token",
    );
    for args in [
        &["events"][..],
        &["installation"],
        &["check-installation"],
        &["agents"],
        &["create-agent"],
        &["revoke-agent", ID],
    ] {
        failure(
            &invoke(command(&url, true), args, None),
            "operation_not_exposed",
        );
    }
    assert!(matches!(listener.accept(), Err(e) if e.kind() == std::io::ErrorKind::WouldBlock));
}

#[test]
fn native_only_routes_get_and_page_reads_and_revoke() {
    for (args, path, method, body) in [
        (
            vec!["get", ID],
            format!("shipments/{ID}"),
            "GET",
            serde_json::to_string(&shipment()).unwrap(),
        ),
        (
            vec!["history", ID, "--limit", "1"],
            format!("shipments/{ID}/events?limit=1"),
            "GET",
            serde_json::to_string(&Page::<Event> {
                items: vec![],
                next_page: Some("next".into()),
            })
            .unwrap(),
        ),
        (
            vec!["delivery-estimates", "--page-token", "next"],
            "delivery-estimates?page_token=next".into(),
            "GET",
            serde_json::to_string(&Page::<DeliveryEstimate> {
                items: vec![],
                next_page: None,
            })
            .unwrap(),
        ),
        (
            vec!["events", "--after", "5", "--limit", "2"],
            "events?after=5&limit=2".into(),
            "GET",
            serde_json::to_string(&StateEventPage {
                items: vec![],
                next_after: 5,
                has_more: false,
            })
            .unwrap(),
        ),
        (
            vec!["carriers"],
            "carriers".into(),
            "GET",
            serde_json::to_string(&vec![Carrier::Usps]).unwrap(),
        ),
        (
            vec!["agents"],
            "agents".into(),
            "GET",
            serde_json::to_string(&AgentCredentials { items: vec![] }).unwrap(),
        ),
        (
            vec!["system"],
            "system".into(),
            "GET",
            serde_json::to_string(&System {
                version: "fixture".into(),
                provider_enabled: false,
                enabled_providers: vec![],
                pending_syncs: 0,
                sync_errors: 0,
                credential_role: "agent".into(),
                delivery_day: None,
            })
            .unwrap(),
        ),
    ] {
        let (url, handle) = fixture(200, "", body);
        let output = invoke(command(&url, false), &args, None);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let request = handle.join().unwrap();
        assert_eq!(request.path, format!("/v1/{path}"));
        assert_eq!(request.method, method);
    }
    let (url, handle) = fixture(204, "", String::new());
    let envelope: Envelope<()> =
        success(&invoke(command(&url, false), &["revoke-agent", ID], None));
    assert_eq!(envelope.status, 204);
    let request = handle.join().unwrap();
    assert_eq!(request.method, "DELETE");
    assert_eq!(request.path, format!("/v1/agents/{ID}"));
}

#[test]
fn known_api_error_codes_survive_but_unknown_text_does_not() {
    for (code, retained) in [
        ("tracking_duplicate", true),
        ("revision_conflict", true),
        ("tracking_verification_pending", true),
        ("tracking_provider_credentials", true),
        ("sensitive-body", false),
    ] {
        let (url, handle) = fixture(
            409,
            "",
            format!(
                r#"{{"error_code":"{code}","message":"sensitive-body {SECRET}","request_id":"sensitive-body"}}"#
            ),
        );
        let output = invoke(command(&url, false), &["get", ID], None);
        failure(&output, "http_error");
        assert_eq!(
            String::from_utf8_lossy(&output.stderr).contains(&format!("\"error_code\":\"{code}\"")),
            retained
        );
        handle.join().unwrap();
    }
}

#[test]
fn selected_transport_never_uses_other_token_and_oversized_input_is_local() {
    let (url, handle) = fixture(
        200,
        "",
        serde_json::to_string(&vec![Carrier::Usps]).unwrap(),
    );
    let mut selected = command(&url, false);
    selected
        .env("HOME_ASSISTANT_URL", "https://sensitive.example")
        .env("HOME_ASSISTANT_TOKEN", "wrong-token");
    success::<Vec<Carrier>>(&invoke(
        selected,
        &["--transport", "native", "carriers"],
        None,
    ));
    assert_eq!(
        handle.join().unwrap().headers["authorization"],
        format!("Bearer {SECRET}")
    );
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let mut file = NamedTempFile::new().unwrap();
    file.write_all(&vec![b' '; 1024 * 1024 + 1]).unwrap();
    failure(
        &invoke(
            command(&url, false),
            &[
                "create",
                "--idempotency-key",
                "one",
                "--input",
                file.path().to_str().unwrap(),
            ],
            None,
        ),
        "input_too_large",
    );
    let mut invalid_token = command(&url, false);
    invalid_token.env("SHIPMENTS_AGENT_TOKEN", "sensitive-body\nsecret");
    failure(&invoke(invalid_token, &["carriers"], None), "invalid_token");
    assert!(matches!(listener.accept(), Err(e) if e.kind() == std::io::ErrorKind::WouldBlock));
}
