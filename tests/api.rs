use dropshot::{HttpServer, ResultsPage};
use reqwest::{Client, Method, Response, StatusCode};
use shipments::{api, models::*, store::Store};
use std::sync::Arc;
use tempfile::TempDir;

const OWNER: &str = "owner-test-credential-at-least-32-bytes";
const AGENT: &str = "agent-test-credential-at-least-32-bytes";
const INTEGRATION: &str = "integration-test-credential-at-least-32-bytes";

struct App {
    _dir: TempDir,
    store: Store,
    server: HttpServer<Arc<api::Context>>,
    client: Client,
    url: String,
}
impl App {
    async fn new(ingress_peer: Option<std::net::IpAddr>) -> Self {
        Self::with_provider(ingress_peer, None).await
    }
    async fn with_provider(
        ingress_peer: Option<std::net::IpAddr>,
        provider: Option<shipments::sync::EasyPostClient>,
    ) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("shipments.sqlite3"))
            .await
            .unwrap();
        let context = Arc::new(
            api::Context::new(store.clone(), OWNER, Some(AGENT), ingress_peer, provider)
                .with_integration(INTEGRATION),
        );
        let server = api::start("127.0.0.1:0".parse().unwrap(), context).unwrap();
        let url = format!("http://{}", server.local_addr());
        Self {
            _dir: dir,
            store,
            server,
            client: Client::new(),
            url,
        }
    }
    fn request(&self, method: Method, path: &str) -> reqwest::RequestBuilder {
        self.client
            .request(method, format!("{}{path}", self.url))
            .bearer_auth(OWNER)
    }
    async fn create(&self, title: &str, key: &str) -> Response {
        self.request(Method::POST, "/v1/shipments")
            .header("Idempotency-Key", key)
            .json(&input(title))
            .send()
            .await
            .unwrap()
    }
    async fn close(self) {
        self.server.close().await.unwrap();
        self.store.close().await.unwrap();
    }
}
fn input(title: &str) -> CreateShipment {
    CreateShipment {
        title: title.into(),
        merchant: None,
        order_reference: None,
        tracking: None,
        source: None,
    }
}

#[tokio::test]
async fn creates_are_durable_repeatable_and_conflict_checked() {
    let app = App::new(None).await;
    let response = app.create("A parcel", "email-create").await;
    assert_eq!(response.status(), StatusCode::CREATED);
    assert_eq!(response.headers()["etag"], "\"1\"");
    let created: Shipment = response.json().await.unwrap();
    let edited: Shipment = app
        .request(Method::PATCH, &format!("/v1/shipments/{}", created.id))
        .header("If-Match", "\"1\"")
        .json(&UpdateShipment {
            title: Some("Corrected title".into()),
            ..Default::default()
        })
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(edited.revision, 2);
    let replay: Shipment = app
        .create("A parcel", "email-create")
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(created.id, replay.id);
    assert_eq!(created.title, replay.title);
    assert_eq!(created.revision, replay.revision);
    assert_eq!(
        app.create("Other parcel", "email-create").await.status(),
        StatusCode::CONFLICT
    );
    let history: ResultsPage<Event> = app
        .request(Method::GET, &format!("/v1/shipments/{}/events", created.id))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(history.items.len(), 2);
    assert_eq!(
        history.items[0].snapshot.as_ref().unwrap().title,
        "A parcel"
    );
    assert_eq!(
        history.items[1].snapshot.as_ref().unwrap().title,
        "Corrected title"
    );
    app.close().await;
}

#[tokio::test]
async fn optimistic_updates_and_null_patch_preserve_newer_state() {
    let app = App::new(None).await;
    let mut request = input("Parcel");
    request.merchant = Some("Shop".into());
    request.tracking = Some(Tracking {
        carrier: Some("UPS".into()),
        number: "1Z ABC".into(),
    });
    let created: Shipment = app
        .request(Method::POST, "/v1/shipments")
        .header("Idempotency-Key", "new")
        .json(&request)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let path = format!("/v1/shipments/{}", created.id);
    assert_eq!(
        app.request(Method::PATCH, &path)
            .json(&UpdateShipment::default())
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::PRECONDITION_REQUIRED
    );
    let patch = UpdateShipment {
        merchant: Patch { value: Some(None) },
        ..Default::default()
    };
    let response = app
        .request(Method::PATCH, &path)
        .header("If-Match", "\"1\"")
        .json(&patch)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let edited: Shipment = response.json().await.unwrap();
    assert!(edited.merchant.is_none());
    assert_eq!(edited.tracking.unwrap().number, "1ZABC");
    assert_eq!(
        app.request(Method::PATCH, &path)
            .header("If-Match", "\"1\"")
            .json(&UpdateShipment::default())
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::PRECONDITION_FAILED
    );
    let edit1 = UpdateShipment {
        title: Some("First edit".into()),
        ..Default::default()
    };
    let edit2 = UpdateShipment {
        title: Some("Second edit".into()),
        ..Default::default()
    };
    let (one, two) = tokio::join!(
        app.request(Method::PATCH, &path)
            .header("If-Match", "\"2\"")
            .json(&edit1)
            .send(),
        app.request(Method::PATCH, &path)
            .header("If-Match", "\"2\"")
            .json(&edit2)
            .send()
    );
    let mut codes = [
        one.unwrap().status().as_u16(),
        two.unwrap().status().as_u16(),
    ];
    codes.sort();
    assert_eq!(codes, [200, 412]);
    app.close().await;
}

#[tokio::test]
async fn owner_actions_are_separate_from_ingestion_identity() {
    let app = App::new(None).await;
    assert_eq!(
        app.client
            .get(format!("{}/v1/summary", app.url))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    let body = input("Agent parcel");
    let shipment: Shipment = app
        .client
        .post(format!("{}/v1/shipments", app.url))
        .bearer_auth(AGENT)
        .header("X-Shipments-Actor", "pretend-owner")
        .header("Idempotency-Key", "agent-add")
        .json(&body)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let correction = SetOverride {
        status: Status::Delivered,
        reason: "Physically received".into(),
    };
    let path = format!("/v1/shipments/{}/status-override", shipment.id);
    assert_eq!(
        app.client
            .put(format!("{}{path}", app.url))
            .bearer_auth(AGENT)
            .header("If-Match", "\"1\"")
            .json(&correction)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    let changed: Shipment = app
        .request(Method::PUT, &path)
        .header("If-Match", "\"1\"")
        .header("X-Shipments-Actor", "ha:human")
        .json(&correction)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(changed.status, Status::Delivered);
    assert_eq!(changed.status_override.unwrap().actor_id, "ha:human");
    let events: ResultsPage<Event> = app
        .request(
            Method::GET,
            &format!("/v1/shipments/{}/events", shipment.id),
        )
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(events.items[0].actor_id, "agent");
    let system: System = app
        .client
        .get(format!("{}/v1/system", app.url))
        .bearer_auth(AGENT)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(system.credential_role, "agent");
    app.close().await;
}

#[tokio::test]
async fn duplicate_sources_tracking_archive_and_pagination_use_real_http() {
    let app = App::new(None).await;
    let mut body = input("First");
    body.tracking = Some(Tracking {
        carrier: Some("UPS".into()),
        number: "TEST001".into(),
    });
    body.source = Some(Source {
        namespace: "test".into(),
        key: "message1".into(),
    });
    let (one, two) = tokio::join!(
        app.request(Method::POST, "/v1/shipments")
            .header("Idempotency-Key", "a")
            .json(&body)
            .send(),
        app.request(Method::POST, "/v1/shipments")
            .header("Idempotency-Key", "b")
            .json(&body)
            .send()
    );
    let mut responses = vec![one.unwrap(), two.unwrap()];
    responses.sort_by_key(|r| r.status().as_u16());
    assert_eq!(responses[1].status(), StatusCode::CONFLICT);
    let created: Shipment = responses.remove(0).json().await.unwrap();
    let mut ambiguous = input("Carrier not known yet");
    ambiguous.tracking = Some(Tracking {
        carrier: None,
        number: "TEST001".into(),
    });
    assert_eq!(
        app.request(Method::POST, "/v1/shipments")
            .header("Idempotency-Key", "unknown-carrier")
            .json(&ambiguous)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::CONFLICT
    );
    body.tracking = None;
    assert_eq!(
        app.request(Method::POST, "/v1/shipments")
            .header("Idempotency-Key", "c")
            .json(&body)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::CONFLICT
    );
    let filtered: ResultsPage<Shipment> = app
        .request(
            Method::GET,
            "/v1/shipments?source_namespace=test&source_key=message1",
        )
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(filtered.items[0].id, created.id);
    for index in 0..3 {
        app.create("Another", &format!("other-{index}")).await;
    }
    let first: ResultsPage<Shipment> = app
        .request(Method::GET, "/v1/shipments?limit=2")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(first.items.len(), 2);
    let token = first.next_page.unwrap();
    let last: ResultsPage<Shipment> = app
        .request(Method::GET, "/v1/shipments")
        .query(&[("limit", "2"), ("page_token", &token)])
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(last.items.len(), 2);
    assert!(last.next_page.is_none());
    assert_ne!(first.items[0].id, last.items[0].id);
    let archive_path = format!("/v1/shipments/{}/archive", created.id);
    let archived: Shipment = app
        .request(Method::PUT, &archive_path)
        .header("If-Match", "\"1\"")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(archived.archived);
    assert_eq!(archived.status, Status::Expected);
    let restored: Shipment = app
        .request(Method::DELETE, &archive_path)
        .header("If-Match", "\"2\"")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(!restored.archived);
    app.close().await;
}

#[tokio::test]
async fn active_views_follow_effective_status_and_keep_archive_separate() {
    let app = App::new(None).await;
    let expected: Shipment = app
        .create("Still expected", "expected")
        .await
        .json()
        .await
        .unwrap();
    let mut ids = Vec::new();
    for (index, status) in Status::TERMINAL.into_iter().enumerate() {
        let shipment: Shipment = app
            .create("Finished", &format!("finished-{index}"))
            .await
            .json()
            .await
            .unwrap();
        let updated: Shipment = app
            .request(
                Method::PUT,
                &format!("/v1/shipments/{}/status-override", shipment.id),
            )
            .header("If-Match", "\"1\"")
            .json(&SetOverride {
                status,
                reason: "Human correction".into(),
            })
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(updated.status, status);
        ids.push(shipment.id);
    }
    let page: ResultsPage<Shipment> = app
        .request(Method::GET, "/v1/shipments?active=true")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        page.items.iter().map(|s| s.id).collect::<Vec<_>>(),
        [expected.id]
    );
    let page: ResultsPage<Shipment> = app
        .request(Method::GET, "/v1/shipments?active=false")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(page.items.iter().map(|s| s.id).collect::<Vec<_>>(), ids);
    let page: ResultsPage<Shipment> = app
        .request(Method::GET, "/v1/shipments?active=false&status=delivered")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0].id, ids[0]);
    let archived = app
        .request(
            Method::PUT,
            &format!("/v1/shipments/{}/archive", expected.id),
        )
        .header("If-Match", "\"1\"")
        .send()
        .await
        .unwrap();
    assert_eq!(archived.status(), StatusCode::OK);
    let page: ResultsPage<Shipment> = app
        .request(Method::GET, "/v1/shipments?active=true&archived=true")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0].id, expected.id);
    let summary: Summary = app
        .request(Method::GET, "/v1/summary")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(summary.active, 0);
    assert_eq!(summary.delivered, 1);
    // Clearing a human correction brings the shipment back into the active view.
    let cleared = app
        .request(
            Method::DELETE,
            &format!("/v1/shipments/{}/status-override", ids[0]),
        )
        .header("If-Match", "\"2\"")
        .send()
        .await
        .unwrap();
    assert_eq!(cleared.status(), StatusCode::OK);
    let page: ResultsPage<Shipment> = app
        .request(Method::GET, "/v1/shipments?active=true")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0].id, ids[0]);
    app.close().await;
}

#[tokio::test]
async fn ingress_checks_real_peer_and_browser_request_header() {
    let forbidden = App::new(Some("172.30.32.2".parse().unwrap())).await;
    assert_eq!(
        forbidden
            .request(Method::GET, "/")
            .header("X-Forwarded-For", "172.30.32.2")
            .header("X-Remote-User-Id", "human")
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    forbidden.close().await;
    let app = App::new(Some("127.0.0.1".parse().unwrap())).await;
    assert_eq!(
        app.request(Method::GET, "/v1/system")
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        app.request(Method::POST, "/v1/shipments")
            .header("X-Remote-User-Id", "human")
            .header("Idempotency-Key", "browser")
            .json(&input("Browser parcel"))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    let response = app
        .request(Method::POST, "/v1/shipments")
        .header("X-Remote-User-Id", "human")
        .header("X-Shipments-Request", "1")
        .header("Idempotency-Key", "browser")
        .json(&input("Browser parcel"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    app.close().await;
}

#[test]
fn generated_openapi_matches_the_committed_contract() {
    let schema = serde_json::to_string_pretty(&api::openapi().unwrap()).unwrap();
    expectorate::assert_contents("openapi/api.json", &schema);
}

#[tokio::test]
async fn integration_credential_is_scoped_and_installation_requires_a_real_check_in() {
    use shipments::installation::{
        BackupReport, BackupStatus, CheckStatus, InstallationReport, IntegrationPresence,
    };

    let app = App::new(None).await;
    let initial: InstallationReport = app
        .request(Method::GET, "/v1/installation")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(initial.backend.status, CheckStatus::Pass);
    assert_eq!(initial.integration.status, CheckStatus::Attention);
    assert_eq!(initial.authentication.status, CheckStatus::Attention);
    assert_eq!(initial.backup.status, CheckStatus::Unavailable);
    assert_eq!(initial.easypost.status, CheckStatus::NotApplicable);
    for credential in [INTEGRATION, AGENT] {
        for (method, path) in [
            (Method::GET, "/v1/installation"),
            (Method::POST, "/v1/installation/check"),
        ] {
            assert_eq!(
                app.client
                    .request(method, format!("{}{path}", app.url))
                    .bearer_auth(credential)
                    .header("X-Shipments-Actor", "ha:test")
                    .send()
                    .await
                    .unwrap()
                    .status(),
                StatusCode::FORBIDDEN
            );
        }
    }
    let system: System = app
        .client
        .get(format!("{}/v1/system", app.url))
        .bearer_auth(INTEGRATION)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(system.credential_role, "integration");
    assert_eq!(
        app.client
            .post(format!("{}/v1/shipments", app.url))
            .bearer_auth(INTEGRATION)
            .header("Idempotency-Key", "no-actor")
            .json(&input("Parcel"))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    let created = app
        .client
        .post(format!("{}/v1/shipments", app.url))
        .bearer_auth(INTEGRATION)
        .header("X-Shipments-Actor", "ha:human")
        .header("Idempotency-Key", "ha-created")
        .json(&input("Parcel"))
        .send()
        .await
        .unwrap();
    assert_eq!(created.status(), StatusCode::CREATED);
    let shipment: Shipment = created.json().await.unwrap();
    let events: ResultsPage<Event> = app
        .request(
            Method::GET,
            &format!("/v1/shipments/{}/events", shipment.id),
        )
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(events.items[0].actor_id, "ha:human");
    let report = IntegrationPresence {
        version: "0.2.0".into(),
        backup: BackupReport {
            status: BackupStatus::Complete,
            last_backup: Some(1_700_000_000),
        },
    };
    for credential in [OWNER, AGENT] {
        assert_eq!(
            app.client
                .post(format!("{}/v1/integration/presence", app.url))
                .bearer_auth(credential)
                .header("X-Shipments-Actor", "ha:integration")
                .json(&report)
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::FORBIDDEN
        );
    }
    let presence = app
        .client
        .post(format!("{}/v1/integration/presence", app.url))
        .bearer_auth(INTEGRATION)
        .header("X-Shipments-Actor", "ha:integration")
        .json(&report)
        .send()
        .await
        .unwrap();
    assert_eq!(presence.status(), StatusCode::NO_CONTENT);
    let response = app
        .request(Method::GET, "/v1/installation")
        .send()
        .await
        .unwrap();
    let text = response.text().await.unwrap();
    assert!(!text.contains(INTEGRATION));
    assert!(!text.contains(OWNER));
    let checked: InstallationReport = serde_json::from_str(&text).unwrap();
    assert_eq!(checked.integration.status, CheckStatus::Pass);
    assert_eq!(checked.authentication.status, CheckStatus::Pass);
    assert_eq!(checked.backup.status, CheckStatus::Pass);
    assert_eq!(checked.last_backup, Some(1_700_000_000));
    app.close().await;
}

#[tokio::test]
async fn explicit_installation_probe_is_read_only_and_concurrent_checks_are_cached() {
    use shipments::{
        installation::{CheckStatus, InstallationReport},
        sync::EasyPostClient,
    };
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let origin = format!("http://{}/", listener.local_addr().unwrap());
    let (sent, received) = std::sync::mpsc::channel();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        let mut request = Vec::new();
        while !request.ends_with(b"\r\n\r\n") {
            let mut byte = [0];
            stream.read_exact(&mut byte).unwrap();
            request.push(byte[0]);
        }
        sent.send(String::from_utf8(request).unwrap()).unwrap();
        let body = r#"{"trackers":[],"has_more":false}"#;
        write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).unwrap();
    });
    let provider = EasyPostClient::for_loopback("fake-easypost-key", &origin).unwrap();
    let app = App::with_provider(None, Some(provider)).await;
    let local: InstallationReport = app
        .request(Method::GET, "/v1/installation")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(local.easypost.status, CheckStatus::Attention);
    assert!(received.try_recv().is_err());
    let first = app.request(Method::POST, "/v1/installation/check").send();
    let second = app.request(Method::POST, "/v1/installation/check").send();
    let (first, second) = tokio::join!(first, second);
    for response in [first.unwrap(), second.unwrap()] {
        assert_eq!(response.status(), StatusCode::OK);
        let checked: InstallationReport = response.json().await.unwrap();
        assert_eq!(checked.easypost.status, CheckStatus::Pass);
    }
    let request = received.recv().unwrap();
    assert!(request.starts_with("GET /trackers?page_size=1 "));
    server.join().unwrap();
    // The one-response server is closed. A further successful check must use
    // the cached result rather than making a second provider request.
    let cached: InstallationReport = app
        .request(Method::POST, "/v1/installation/check")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(cached.easypost.status, CheckStatus::Pass);
    app.close().await;
}
