use dropshot::{HttpServer, ResultsPage};
use reqwest::{Client, Method, Response, StatusCode};
use shipments::{api, models::*, store::Store};
use std::sync::Arc;
use tempfile::TempDir;

const OWNER: &str = "owner-test-credential-at-least-32-bytes";
const INTEGRATION: &str = "integration-test-credential-at-least-32-bytes";

struct App {
    _dir: TempDir,
    store: Store,
    server: HttpServer<Arc<api::Context>>,
    client: Client,
    url: String,
    agent: String,
    agent_id: uuid::Uuid,
    provider_fixture: Option<ProviderFixture>,
}
impl App {
    async fn new(ingress_peer: Option<std::net::IpAddr>) -> Self {
        let fixture = ProviderFixture::new();
        let mut app = Self::with_provider(ingress_peer, Some(fixture.client.clone())).await;
        app.provider_fixture = Some(fixture);
        app
    }
    async fn with_provider(
        ingress_peer: Option<std::net::IpAddr>,
        provider: Option<shipments::shipping_api::easypost::EasyPostClient>,
    ) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("shipments.sqlite3"))
            .await
            .unwrap();
        let context = Arc::new(
            api::Context::new(
                store.clone(),
                OWNER,
                None,
                provider.map(|provider| {
                    std::sync::Arc::new(provider)
                        as std::sync::Arc<dyn shipments::shipping_api::ShippingApi>
                }),
            )
            .with_integration(INTEGRATION),
        );
        let server = api::start("127.0.0.1:0".parse().unwrap(), context.clone()).unwrap();
        let url = format!("http://{}", server.local_addr());
        let created = Client::new()
            .post(format!("{url}/v1/agents"))
            .bearer_auth(OWNER)
            .header("X-Remote-User-Id", "human")
            .header("X-Shipments-Request", "1")
            .json(&CreateAgent {
                name: "Test importer".into(),
            })
            .send()
            .await
            .unwrap();
        assert_eq!(created.status(), StatusCode::CREATED);
        let created: CreatedAgentCredential = created.json().await.unwrap();
        if ingress_peer.is_some() {
            let report = shipments::installation::IntegrationPresence {
                version: env!("CARGO_PKG_VERSION").into(),
                administrator_ids: vec!["human".into()],
                backup: shipments::installation::BackupReport {
                    status: shipments::installation::BackupStatus::Standalone,
                    last_backup: None,
                },
            };
            let response = Client::new()
                .post(format!("{url}/v1/integration/presence"))
                .bearer_auth(INTEGRATION)
                .header("X-Shipments-Actor", "ha:integration")
                .json(&report)
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::NO_CONTENT);
        }
        let server = if let Some(peer) = ingress_peer {
            server.close().await.unwrap();
            api::start(
                "127.0.0.1:0".parse().unwrap(),
                Arc::new((*context).clone().for_ingress(peer)),
            )
            .unwrap()
        } else {
            server
        };
        let url = format!("http://{}", server.local_addr());
        Self {
            provider_fixture: None,
            agent: created.token,
            agent_id: created.credential.id,
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
        let mut body = input(title);
        body.tracking.number = format!(
            "TEST{}123",
            key.bytes()
                .filter(u8::is_ascii_alphanumeric)
                .map(char::from)
                .collect::<String>()
                .to_uppercase()
        );
        self.request(Method::POST, "/v1/shipments")
            .header("Idempotency-Key", key)
            .json(&body)
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
        direction: shipments::models::Direction::Inbound,
        title: title.into(),
        merchant: None,
        order_reference: None,
        signature_required: None,
        tracking: Tracking {
            carrier: Some("usps".parse().unwrap()),
            number: format!(
                "TEST{}123",
                title
                    .bytes()
                    .filter(u8::is_ascii_alphanumeric)
                    .map(char::from)
                    .collect::<String>()
                    .to_uppercase()
            ),
        },
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
    request.tracking = Tracking {
        carrier: Some("UPS".parse().unwrap()),
        number: "1Z ABC123".into(),
    };
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
    assert_eq!(edited.tracking.number, "1ZABC123");
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
        .bearer_auth(&app.agent)
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
            .bearer_auth(&app.agent)
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
    assert_eq!(events.items[0].actor_id, format!("agent:{}", app.agent_id));
    let system: System = app
        .client
        .get(format!("{}/v1/system", app.url))
        .bearer_auth(&app.agent)
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
    body.tracking = Tracking {
        carrier: Some("UPS".parse().unwrap()),
        number: "TEST001".into(),
    };
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
    let mut responses = Vec::new();
    for response in [one, two] {
        responses.push(response.unwrap());
    }
    responses.sort_by_key(|r| r.status().as_u16());
    assert_eq!(responses[1].status(), StatusCode::CONFLICT);
    let created: Shipment = responses.remove(0).json().await.unwrap();
    let mut ambiguous = input("Carrier not known yet");
    ambiguous.tracking = Tracking {
        carrier: None,
        number: "TEST001".into(),
    };
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
    body.tracking = input("Other tracking").tracking;
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
    assert_eq!(initial.easypost.status, CheckStatus::Attention);
    for credential in [INTEGRATION, &app.agent] {
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
        administrator_ids: Vec::new(),
        version: "0.2.0".into(),
        backup: BackupReport {
            status: BackupStatus::Complete,
            last_backup: Some(1_700_000_000),
        },
    };
    for credential in [OWNER, &app.agent] {
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
        shipping_api::easypost::EasyPostClient,
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
        assert_eq!(
            checked.easypost.status,
            CheckStatus::Pass,
            "{}",
            checked.easypost.message
        );
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

#[tokio::test]
async fn named_agents_are_private_individually_revocable_and_cannot_escalate() {
    let app = App::new(None).await;
    let created = app
        .request(Method::POST, "/v1/agents")
        .json(&CreateAgent {
            name: "  Email importer  ".into(),
        })
        .send()
        .await
        .unwrap();
    assert_eq!(created.status(), StatusCode::CREATED);
    assert_eq!(created.headers()["cache-control"], "no-store");
    let created: CreatedAgentCredential = created.json().await.unwrap();
    assert_eq!(created.credential.name, "Email importer");
    assert_ne!(created.credential.id, app.agent_id);
    assert!(created.token.starts_with("shp_"));
    assert_eq!(created.token.len(), 68);
    assert_ne!(created.token, app.agent);

    let listed = app.request(Method::GET, "/v1/agents").send().await.unwrap();
    assert_eq!(listed.headers()["cache-control"], "no-store");
    let text = listed.text().await.unwrap();
    assert!(!text.contains(&created.token));
    assert!(!text.contains(&app.agent));
    let listed: AgentCredentials = serde_json::from_str(&text).unwrap();
    assert_eq!(listed.items.len(), 2);
    assert_eq!(listed.items[0].id, created.credential.id);
    let stored: Vec<u8> =
        sqlx::query_scalar("SELECT token_digest FROM agent_credentials WHERE id = ?")
            .bind(created.credential.id.to_string())
            .fetch_one(app.store.pool())
            .await
            .unwrap();
    assert_eq!(stored.len(), 32);
    assert_ne!(stored, created.token.as_bytes());

    let path = format!("/v1/agents/{}", created.credential.id);
    for credential in [&app.agent, &created.token, INTEGRATION] {
        for (method, endpoint) in [
            (Method::GET, "/v1/agents"),
            (Method::POST, "/v1/agents"),
            (Method::DELETE, &path),
        ] {
            let response = app
                .client
                .request(method, format!("{}{endpoint}", app.url))
                .bearer_auth(credential)
                .header("X-Shipments-Actor", "ha:pretend-owner")
                .json(&CreateAgent {
                    name: "Not allowed".into(),
                })
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::FORBIDDEN);
        }
    }
    let shipment: Shipment = app
        .client
        .post(format!("{}/v1/shipments", app.url))
        .bearer_auth(&created.token)
        .header("Idempotency-Key", "named-agent")
        .header("X-Shipments-Actor", "owner")
        .json(&input("Imported parcel"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
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
    assert_eq!(
        events.items[0].actor_id,
        format!("agent:{}", created.credential.id)
    );
    assert_eq!(
        app.client
            .put(format!("{}/v1/shipments/{}/archive", app.url, shipment.id))
            .bearer_auth(&created.token)
            .header("If-Match", "\"1\"")
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );

    assert_eq!(
        app.request(Method::DELETE, &path)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        app.request(Method::DELETE, &path)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );
    // Revocation affects the next request without restarting either listener.
    for credential in [&created.token, "shp_invalid"] {
        assert_eq!(
            app.client
                .get(format!("{}/v1/summary", app.url))
                .bearer_auth(credential)
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::UNAUTHORIZED
        );
    }
    for credential in [&app.agent, OWNER, INTEGRATION] {
        assert_eq!(
            app.client
                .get(format!("{}/v1/summary", app.url))
                .bearer_auth(credential)
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::OK
        );
    }
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
    assert_eq!(
        events.items[0].actor_id,
        format!("agent:{}", created.credential.id)
    );
    app.close().await;
}

#[tokio::test]
async fn agent_names_and_concurrent_creation_limit_are_validated() {
    let app = App::new(None).await;
    for name in [
        "".into(),
        "   ".into(),
        "line\nbreak".into(),
        "a".repeat(101),
        "🦆".repeat(26),
    ] {
        assert_eq!(
            app.request(Method::POST, "/v1/agents")
                .json(&CreateAgent { name })
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::BAD_REQUEST
        );
    }
    for index in 1..31 {
        assert_eq!(
            app.request(Method::POST, "/v1/agents")
                .json(&CreateAgent {
                    name: format!("Importer {index}")
                })
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::CREATED
        );
    }
    let name = CreateAgent {
        name: "Last slot".into(),
    };
    let (first, second) = tokio::join!(
        app.request(Method::POST, "/v1/agents").json(&name).send(),
        app.request(Method::POST, "/v1/agents").json(&name).send()
    );
    let mut statuses = [
        first.unwrap().status().as_u16(),
        second.unwrap().status().as_u16(),
    ];
    statuses.sort();
    assert_eq!(statuses, [201, 409]);
    let agents: AgentCredentials = app
        .request(Method::GET, "/v1/agents")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(agents.items.len(), 32);
    assert_eq!(
        app.request(Method::DELETE, &format!("/v1/agents/{}", app.agent_id))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        app.request(Method::POST, "/v1/agents")
            .json(&name)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::CREATED
    );
    app.close().await;
}

#[tokio::test]
async fn ingress_agent_management_requires_the_real_peer_user_and_browser_header() {
    let app = App::new(Some("127.0.0.1".parse().unwrap())).await;
    assert_eq!(
        app.request(Method::POST, "/v1/agents")
            .header("X-Shipments-Request", "1")
            .json(&CreateAgent {
                name: "Importer".into()
            })
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        app.request(Method::POST, "/v1/agents")
            .header("X-Remote-User-Id", "human")
            .json(&CreateAgent {
                name: "Importer".into()
            })
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    let response = app
        .request(Method::POST, "/v1/agents")
        .header("X-Remote-User-Id", "human")
        .header("X-Shipments-Request", "1")
        .json(&CreateAgent {
            name: "Importer".into(),
        })
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let created: CreatedAgentCredential = response.json().await.unwrap();
    assert_eq!(
        app.request(
            Method::DELETE,
            &format!("/v1/agents/{}", created.credential.id)
        )
        .header("X-Remote-User-Id", "human")
        .send()
        .await
        .unwrap()
        .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        app.request(
            Method::DELETE,
            &format!("/v1/agents/{}", created.credential.id)
        )
        .header("X-Remote-User-Id", "human")
        .header("X-Shipments-Request", "1")
        .send()
        .await
        .unwrap()
        .status(),
        StatusCode::NO_CONTENT
    );
    app.close().await;
}

#[derive(serde::Deserialize)]
struct RegisterRequest {
    tracker: RegisterInput,
}
#[derive(Clone, serde::Deserialize)]
struct RegisterInput {
    tracking_code: String,
    carrier: Option<String>,
}
#[derive(serde::Serialize)]
struct RegisteredTracker {
    id: String,
    tracking_code: String,
    carrier: String,
    status: Status,
    public_url: String,
}
struct ProviderFixture {
    client: shipments::shipping_api::easypost::EasyPostClient,
    state: Arc<std::sync::Mutex<ProviderState>>,
    stop: Arc<std::sync::atomic::AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
    address: std::net::SocketAddr,
}
#[derive(Default)]
struct ProviderState {
    requests: Vec<Method>,
    registration_replies: std::collections::VecDeque<(u16, String)>,
}

#[derive(serde::Serialize)]
struct EmptyTrackerPage {
    trackers: Vec<RegisteredTracker>,
    has_more: bool,
}
impl ProviderFixture {
    fn new() -> Self {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let state = Arc::new(std::sync::Mutex::new(ProviderState::default()));
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let thread_state = state.clone();
        let thread_stop = stop.clone();
        let thread = std::thread::spawn(move || {
            for stream in listener.incoming() {
                let mut stream = stream.unwrap();
                if thread_stop.load(std::sync::atomic::Ordering::Acquire) {
                    break;
                }
                stream
                    .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                    .unwrap();
                let mut bytes = Vec::new();
                while !bytes.ends_with(b"\r\n\r\n") {
                    let mut byte = [0];
                    stream.read_exact(&mut byte).unwrap();
                    bytes.push(byte[0]);
                }
                let headers = String::from_utf8(bytes).unwrap();
                let mut line = headers.lines().next().unwrap().split_whitespace();
                let method: Method = line.next().unwrap().parse().unwrap();
                let path = line.next().unwrap();
                let length: usize = headers
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length: ")
                            .map(str::to_owned)
                    })
                    .map_or(0, |length| length.parse().unwrap());
                let mut bytes = vec![0; length];
                stream.read_exact(&mut bytes).unwrap();
                let mut state = thread_state.lock().unwrap();
                state.requests.push(method.clone());
                let (status, body) = if method == Method::GET {
                    let url = reqwest::Url::parse(&format!("http://provider{path}")).unwrap();
                    assert_eq!(url.path(), "/trackers");
                    let query: std::collections::HashMap<_, _> = url.query_pairs().collect();
                    assert!(query.contains_key("tracking_codes[]"));
                    assert!(query.contains_key("start_datetime"));
                    assert!(query.contains_key("end_datetime"));
                    assert!(bytes.is_empty());
                    (
                        200,
                        serde_json::to_string(&EmptyTrackerPage {
                            trackers: Vec::new(),
                            has_more: false,
                        })
                        .unwrap(),
                    )
                } else {
                    assert_eq!(method, Method::POST);
                    assert_eq!(path, "/trackers");
                    let request: RegisterRequest = serde_json::from_slice(&bytes).unwrap();
                    state.registration_replies.pop_front().unwrap_or_else(|| {
                        (
                            200,
                            serde_json::to_string(&RegisteredTracker {
                                id: format!("trk_{}", request.tracker.tracking_code),
                                tracking_code: request.tracker.tracking_code,
                                carrier: request.tracker.carrier.unwrap_or_else(|| "UPS".into()),
                                status: Status::Unknown,
                                public_url: "https://track.easypost.com/test-parcel".into(),
                            })
                            .unwrap(),
                        )
                    })
                };
                drop(state);
                write!(stream, "HTTP/1.1 {status} response\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
            }
        });
        Self {
            client: shipments::shipping_api::easypost::EasyPostClient::for_loopback(
                "disposable-test-key",
                &format!("http://{address}/"),
            )
            .unwrap(),
            state,
            stop,
            thread: Some(thread),
            address,
        }
    }
    fn calls(&self) -> usize {
        self.state.lock().unwrap().requests.len()
    }
    fn methods(&self) -> Vec<Method> {
        self.state.lock().unwrap().requests.clone()
    }
    fn reply_to_registration(&self, status: u16, body: impl Into<String>) {
        self.state
            .lock()
            .unwrap()
            .registration_replies
            .push_back((status, body.into()));
    }
}
impl Drop for ProviderFixture {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Release);
        let _ = std::net::TcpStream::connect(self.address);
        if let Some(thread) = self.thread.take() {
            thread.join().unwrap();
        }
    }
}

#[derive(serde::Deserialize)]
struct ApiFailure {
    error_code: Option<String>,
    message: String,
}

#[tokio::test]
async fn every_api_role_rejects_missing_or_invalid_local_tracking_before_provider_io() {
    let app = App::new(None).await;
    let provider = app.provider_fixture.as_ref().unwrap();
    for credential in [OWNER, &app.agent, INTEGRATION] {
        for body in [
            r#"{"direction":"inbound","title":"Parcel"}"#,
            r#"{"direction":"inbound","title":"Parcel","tracking":null}"#,
            r#"{"direction":"inbound","title":"Parcel","tracking":{"number":"  "}}"#,
        ] {
            let response = app
                .client
                .post(format!("{}/v1/shipments", app.url))
                .bearer_auth(credential)
                .header("X-Shipments-Actor", "ha:test")
                .header("Idempotency-Key", "missing-number")
                .header("Content-Type", "application/json")
                .body(body)
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
            let error: ApiFailure = response.json().await.unwrap();
            assert_eq!(error.error_code.as_deref(), Some("tracking_required"));
            assert!(error.message.contains("tracking number is required"));
        }
        for (number, code) in [
            ("PRIVATELETTERS", "tracking_invalid"),
            ("123", "tracking_invalid"),
            ("PRIVATE-123!", "tracking_invalid"),
            ("😈123456", "tracking_invalid"),
        ] {
            let mut body = input("Locally rejected parcel");
            body.tracking.number = number.into();
            let response = app
                .client
                .post(format!("{}/v1/shipments", app.url))
                .bearer_auth(credential)
                .header("X-Shipments-Actor", "ha:test")
                .header("Idempotency-Key", "invalid-number")
                .json(&body)
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
            let error: ApiFailure = response.json().await.unwrap();
            assert_eq!(error.error_code.as_deref(), Some(code));
            assert!(error.message.to_lowercase().contains("tracking number"));
            assert!(!error.message.contains(number));
        }
    }
    assert_eq!(provider.calls(), 0);
    let summary: Summary = app
        .request(Method::GET, "/v1/summary")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(summary.active, 0);
    let persisted: i64 = sqlx::query_scalar("SELECT count(*) FROM idempotency")
        .fetch_one(app.store.pool())
        .await
        .unwrap();
    assert_eq!(persisted, 0);
    app.close().await;
}

#[tokio::test]
async fn unscanned_tracking_is_saved_pending_then_registered_and_replays_make_no_requests() {
    let app = App::new(None).await;
    let provider = app.provider_fixture.as_ref().unwrap();
    let worker = shipments::sync::SyncWorker::new(
        app.store.pool().clone(),
        std::sync::Arc::new(provider.client.clone()),
    );
    for (index, status) in [Status::Unknown, Status::PreTransit]
        .into_iter()
        .enumerate()
    {
        let mut body = input(&format!("PENDING{index}"));
        body.tracking.carrier = None;
        provider.reply_to_registration(
            200,
            serde_json::to_string(&RegisteredTracker {
                id: format!("trk_PENDING{index}"),
                tracking_code: body.tracking.number.clone(),
                carrier: "USPS".into(),
                status,
                public_url: format!("https://track.easypost.com/pending-{index}"),
            })
            .unwrap(),
        );
        let key = format!("unscanned-{index}");
        let created = app
            .request(Method::POST, "/v1/shipments")
            .header("Idempotency-Key", &key)
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(created.status(), StatusCode::CREATED);
        let created: Shipment = created.json().await.unwrap();
        assert_eq!(created.status, Status::Expected);
        assert_eq!(created.sync.registration_state, RegistrationState::Pending);
        assert!(created.tracking.carrier.is_none());
        assert!(created.sync.tracker_id.is_none());
        assert!(created.sync.last_success_at.is_none());
        assert_eq!(provider.calls(), index * 2);
        assert!(worker.tick().await.unwrap());
        let registered: Shipment = app
            .request(Method::GET, &format!("/v1/shipments/{}", created.id))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(registered.status, status);
        assert_eq!(
            registered.sync.registration_state,
            RegistrationState::Registered
        );
        assert_eq!(registered.tracking.carrier, Some("USPS".parse().unwrap()));
        assert_eq!(
            registered.sync.tracking_url.as_deref(),
            Some(format!("https://track.easypost.com/pending-{index}").as_str())
        );
        assert!(registered.sync.last_success_at.is_some());
        let replay = app
            .request(Method::POST, "/v1/shipments")
            .header("Idempotency-Key", &key)
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(replay.status(), StatusCode::CREATED);
        let replay: Shipment = replay.json().await.unwrap();
        assert_eq!(replay.id, created.id);
        assert_eq!(replay.revision, created.revision);
        assert_eq!(replay.sync.registration_state, RegistrationState::Pending);
        assert_eq!(provider.calls(), (index + 1) * 2);
        let edit = app
            .request(Method::PATCH, &format!("/v1/shipments/{}", created.id))
            .header("If-Match", format!("\"{}\"", registered.revision))
            .json(&UpdateShipment {
                title: Some("Metadata edit".into()),
                tracking: Patch {
                    value: Some(Some(registered.tracking.clone())),
                },
                ..Default::default()
            })
            .send()
            .await
            .unwrap();
        assert_eq!(edit.status(), StatusCode::OK);
        let edited: Shipment = edit.json().await.unwrap();
        assert_eq!(
            edited.sync.registration_state,
            RegistrationState::Registered
        );
        assert_eq!(edited.sync.tracker_id, registered.sync.tracker_id);
        assert_eq!(provider.calls(), (index + 1) * 2);
    }
    assert!(!worker.tick().await.unwrap());
    assert_eq!(provider.calls(), 4);
    assert_eq!(
        provider.methods(),
        [Method::GET, Method::POST, Method::GET, Method::POST]
    );
    app.close().await;
}

#[tokio::test]
async fn tracking_changes_save_pending_and_later_provider_rejection_preserves_the_new_input() {
    let app = App::new(None).await;
    let provider = app.provider_fixture.as_ref().unwrap();
    let worker = shipments::sync::SyncWorker::new(
        app.store.pool().clone(),
        std::sync::Arc::new(provider.client.clone()),
    );
    let created: Shipment = app
        .create("Parcel", "identity-change")
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(provider.calls(), 0);
    assert!(worker.tick().await.unwrap());
    let registered = app.store.get(created.id).await.unwrap();
    let path = format!("/v1/shipments/{}", created.id);
    let version = format!("\"{}\"", registered.revision);
    let cleared = app
        .request(Method::PATCH, &path)
        .header("If-Match", &version)
        .json(&UpdateShipment {
            tracking: Patch { value: Some(None) },
            ..Default::default()
        })
        .send()
        .await
        .unwrap();
    assert_eq!(cleared.status(), StatusCode::BAD_REQUEST);
    let error: ApiFailure = cleared.json().await.unwrap();
    assert_eq!(error.error_code.as_deref(), Some("tracking_required"));
    assert_eq!(provider.calls(), 2);
    let changed = UpdateShipment {
        tracking: Patch {
            value: Some(Some(Tracking {
                carrier: Some("ups".parse().unwrap()),
                number: "NEWNUMBER123".into(),
            })),
        },
        ..Default::default()
    };
    let accepted = app
        .request(Method::PATCH, &path)
        .header("If-Match", &version)
        .json(&changed)
        .send()
        .await
        .unwrap();
    assert_eq!(accepted.status(), StatusCode::OK);
    let accepted: Shipment = accepted.json().await.unwrap();
    assert_eq!(accepted.tracking.number, "NEWNUMBER123");
    assert_eq!(accepted.sync.registration_state, RegistrationState::Pending);
    assert!(accepted.sync.tracker_id.is_none());
    assert!(accepted.sync.last_success_at.is_none());
    assert_eq!(accepted.revision, registered.revision + 1);
    assert_eq!(provider.calls(), 2);
    provider.reply_to_registration(
        422,
        r#"{"error":{"code":"TRACKER.CARRIER_CODE_MISMATCH","message":"private-provider-detail"}}"#,
    );
    assert!(worker.tick().await.unwrap());
    let rejected: Shipment = app
        .request(Method::GET, &path)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(rejected.tracking, accepted.tracking);
    assert_eq!(rejected.revision, accepted.revision);
    assert_eq!(
        rejected.sync.registration_state,
        RegistrationState::Rejected
    );
    assert_eq!(
        rejected.sync.error_code.as_deref(),
        Some("tracking_carrier_mismatch")
    );
    assert_eq!(rejected.sync.provider_status, Some(422));
    let message = rejected.sync.last_error.as_deref().unwrap();
    assert!(message.contains("Check the number and carrier"));
    assert!(!message.contains("private-provider-detail"));
    assert!(!message.contains("NEWNUMBER123"));
    assert!(rejected.sync.next_attempt_at.is_none());
    assert_eq!(provider.calls(), 4);
    let stale = app
        .request(Method::PATCH, &path)
        .header("If-Match", &version)
        .json(&changed)
        .send()
        .await
        .unwrap();
    assert_eq!(stale.status(), StatusCode::PRECONDITION_FAILED);
    assert!(!worker.tick().await.unwrap());
    assert_eq!(provider.calls(), 4);
    assert_eq!(
        provider.methods(),
        [Method::GET, Method::POST, Method::GET, Method::POST]
    );
    app.close().await;
}

#[tokio::test]
async fn provider_outage_does_not_discard_a_valid_create() {
    let app = App::new(None).await;
    app.provider_fixture
        .as_ref()
        .unwrap()
        .reply_to_registration(503, "unavailable");
    let response = app.create("Parcel503", "pending503").await;
    assert_eq!(
        response.status(),
        StatusCode::CREATED,
        "valid parcels must be durably saved during provider outages"
    );
    let shipment: Shipment = response.json().await.unwrap();
    assert_eq!(
        app.store.get(shipment.id).await.unwrap().tracking,
        shipment.tracking
    );
    app.close().await;
}

#[tokio::test]
async fn provider_failures_are_visible_on_durable_records_with_safe_codes_and_http_status() {
    for (status, body, code, state, provider_status) in [
        (
            422,
            r#"{"error":{"code":"TRACKER.NO_CARRIER_FOR_CODE","message":"private-provider-detail"}}"#,
            "tracking_unsupported",
            RegistrationState::Rejected,
            Some(422),
        ),
        (
            422,
            r#"{"error":{"code":"TRACKER.MULTIPLE_CARRIERS_FOR_CODE"}}"#,
            "tracking_carrier_ambiguous",
            RegistrationState::Rejected,
            Some(422),
        ),
        (
            422,
            r#"{"error":{"code":"TRACKER.INVALID_PARAMS"}}"#,
            "tracking_invalid",
            RegistrationState::Rejected,
            Some(422),
        ),
        (
            422,
            r#"{"error":{"code":"TRACKER.INVALID_TEST_CODE"}}"#,
            "tracking_test_key",
            RegistrationState::Pending,
            Some(422),
        ),
        (
            400,
            r#"{"error":{"code":"TRACKER.NO_CARRIER_ACCOUNT"}}"#,
            "tracking_carrier_account_required",
            RegistrationState::Pending,
            Some(400),
        ),
        (
            401,
            "private-provider-detail",
            "tracking_provider_credentials",
            RegistrationState::Pending,
            Some(401),
        ),
        (
            403,
            "private-provider-detail",
            "tracking_provider_credentials",
            RegistrationState::Pending,
            Some(403),
        ),
        (
            429,
            "private-provider-detail",
            "tracking_verification_rate_limited",
            RegistrationState::Pending,
            Some(429),
        ),
        (
            503,
            "private-provider-detail",
            "provider_http",
            RegistrationState::Pending,
            Some(503),
        ),
        (
            200,
            "malformed-provider-response",
            "provider_invalid_response",
            RegistrationState::Pending,
            None,
        ),
    ] {
        // Each registration POST follows a complete, empty lookup. Its response
        // is independent; a 429 pauses all operations sharing this client.
        let app = App::new(None).await;
        let provider = app.provider_fixture.as_ref().unwrap();
        provider.reply_to_registration(status, body);
        let response = app.create("Parcel", "durable-input").await;
        assert_eq!(response.status(), StatusCode::CREATED);
        let created: Shipment = response.json().await.unwrap();
        assert_eq!(created.sync.registration_state, RegistrationState::Pending);
        assert_eq!(provider.calls(), 0);
        let worker = shipments::sync::SyncWorker::new(
            app.store.pool().clone(),
            std::sync::Arc::new(provider.client.clone()),
        );
        assert!(worker.tick().await.unwrap());
        let persisted: Shipment = app
            .request(Method::GET, &format!("/v1/shipments/{}", created.id))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(persisted.tracking, created.tracking);
        assert_eq!(persisted.sync.registration_state, state);
        assert_eq!(persisted.sync.error_code.as_deref(), Some(code));
        assert_eq!(persisted.sync.provider_status, provider_status);
        assert_eq!(persisted.sync.attempts, 1);
        let message = persisted.sync.last_error.as_deref().unwrap();
        assert!(!message.contains("private-provider-detail"));
        assert!(!message.contains(&created.tracking.number));
        if code == "tracking_unsupported" {
            assert_eq!(
                persisted.sync.provider_error_code.as_deref(),
                Some("TRACKER.NO_CARRIER_FOR_CODE")
            );
            assert!(message.contains("configured provider (EasyPost)"));
            assert!(message.contains("This does not mean the tracking number is invalid."));
            assert!(message.contains("Check provider support and the selected carrier"));
        }
        assert_eq!(
            persisted.sync.next_attempt_at.is_some(),
            state == RegistrationState::Pending
        );
        let replay: Shipment = app
            .create("Parcel", "durable-input")
            .await
            .json()
            .await
            .unwrap();
        assert_eq!(replay.id, created.id);
        assert_eq!(provider.calls(), 2);
        assert_eq!(provider.methods(), [Method::GET, Method::POST]);
        let summary: Summary = app
            .request(Method::GET, "/v1/summary")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(summary.active, 1);
        assert_eq!(summary.sync_errors, 1);
        app.close().await;
    }
}

#[tokio::test]
async fn shared_provider_cooldown_keeps_new_api_creates_durable_and_worker_requests_paused() {
    let app = App::new(None).await;
    let provider = app.provider_fixture.as_ref().unwrap();
    let worker = shipments::sync::SyncWorker::new(
        app.store.pool().clone(),
        std::sync::Arc::new(provider.client.clone()),
    );
    let response = app.create("Limited parcel", "limited").await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let first: Shipment = response.json().await.unwrap();
    provider.reply_to_registration(429, "private-provider-detail");
    assert!(worker.tick().await.unwrap());
    let limited = app.store.get(first.id).await.unwrap();
    assert_eq!(limited.sync.provider_status, Some(429));
    assert!(limited.sync.next_attempt_at.is_some());
    assert_eq!(provider.calls(), 2);
    assert_eq!(provider.methods(), [Method::GET, Method::POST]);

    for (index, credential) in [OWNER, &app.agent, INTEGRATION].into_iter().enumerate() {
        let response = app
            .request(Method::POST, "/v1/shipments")
            .bearer_auth(credential)
            .header("X-Shipments-Actor", "ha:test")
            .header(
                "Idempotency-Key",
                format!("queued-during-provider-pause-{index}"),
            )
            .json(&input(&format!("Valid unscanned parcel{index}")))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
        let created: Shipment = response.json().await.unwrap();
        assert_eq!(created.sync.registration_state, RegistrationState::Pending);
        assert!(created.sync.last_error.is_none());
        assert!(created.sync.error_code.is_none());
        assert_eq!(created.sync.attempts, 0);
        assert!(!worker.tick().await.unwrap());
        let due: bool =
            sqlx::query_scalar("SELECT next_attempt_at<=unixepoch() FROM shipments WHERE id=?")
                .bind(created.id.to_string())
                .fetch_one(app.store.pool())
                .await
                .unwrap();
        assert!(due);
    }
    assert_eq!(provider.calls(), 2);
    assert_eq!(provider.methods(), [Method::GET, Method::POST]);
    let summary: Summary = app
        .request(Method::GET, "/v1/summary")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(summary.active, 4);
    assert_eq!(summary.sync_errors, 1);
    app.close().await;
}

#[tokio::test]
async fn direction_is_required_and_invalid_values_never_reach_the_provider() {
    let app = App::new(None).await;
    for body in [
        r#"{"title":"Parcel","tracking":{"number":"DIRECTION-TEST"}}"#,
        r#"{"title":"Parcel","direction":null,"tracking":{"number":"DIRECTION-TEST"}}"#,
        r#"{"title":"Parcel","direction":"PRIVATE-DIRECTION-SENTINEL","tracking":{"number":"DIRECTION-TEST"}}"#,
        r#"{"title":"Parcel","direction":42,"tracking":{"number":"DIRECTION-TEST"}}"#,
    ] {
        let response = app
            .request(Method::POST, "/v1/shipments")
            .header("Idempotency-Key", "invalid-direction")
            .header("Content-Type", "application/json")
            .body(body)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let error: ApiFailure = response.json().await.unwrap();
        assert!(error.message.contains("direction"));
        assert!(!error.message.contains("PRIVATE-DIRECTION-SENTINEL"));
    }
    assert_eq!(app.provider_fixture.as_ref().unwrap().calls(), 0);
    app.close().await;
}

#[tokio::test]
async fn owners_and_agents_create_and_change_direction_without_reverifying_tracking() {
    let app = App::new(None).await;
    let mut request = input("Outbound direction parcel");
    request.direction = Direction::Outbound;
    let created: Shipment = app
        .request(Method::POST, "/v1/shipments")
        .bearer_auth(&app.agent)
        .header("Idempotency-Key", "outbound-create")
        .json(&request)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(created.direction, Direction::Outbound);
    let replay: Shipment = app
        .request(Method::POST, "/v1/shipments")
        .bearer_auth(&app.agent)
        .header("Idempotency-Key", "outbound-create")
        .json(&request)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(replay.id, created.id);
    assert_eq!(replay.direction, Direction::Outbound);
    request.direction = Direction::Inbound;
    assert_eq!(
        app.request(Method::POST, "/v1/shipments")
            .bearer_auth(&app.agent)
            .header("Idempotency-Key", "outbound-create")
            .json(&request)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::CONFLICT
    );
    let path = format!("/v1/shipments/{}", created.id);
    let changed: Shipment = app
        .request(Method::PATCH, &path)
        .bearer_auth(&app.agent)
        .header("If-Match", "\"1\"")
        .json(&UpdateShipment {
            direction: Patch {
                value: Some(Some(Direction::Inbound)),
            },
            ..Default::default()
        })
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(changed.direction, Direction::Inbound);
    assert_eq!(changed.revision, 2);
    assert_eq!(app.provider_fixture.as_ref().unwrap().calls(), 0);
    let null = app
        .request(Method::PATCH, &path)
        .header("If-Match", "\"2\"")
        .header("Content-Type", "application/json")
        .body(r#"{"direction":null}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(null.status(), StatusCode::BAD_REQUEST);
    let error: ApiFailure = null.json().await.unwrap();
    assert!(error.message.contains("direction is required"));
    let preserved: Shipment = app
        .request(Method::PATCH, &path)
        .header("If-Match", "\"2\"")
        .json(&UpdateShipment {
            title: Some("Direction stays inbound".into()),
            ..Default::default()
        })
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(preserved.direction, Direction::Inbound);
    let history: ResultsPage<Event> = app
        .request(Method::GET, &format!("{path}/events"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let directions: Vec<_> = history
        .items
        .into_iter()
        .map(|event| event.snapshot.unwrap().direction)
        .collect();
    assert_eq!(
        directions,
        vec![Direction::Outbound, Direction::Inbound, Direction::Inbound]
    );
    assert_eq!(app.provider_fixture.as_ref().unwrap().calls(), 0);
    app.close().await;
}

#[tokio::test]
async fn ordinary_ingress_users_can_read_but_cannot_claim_administrator_permissions() {
    let app = App::new(Some("127.0.0.1".parse().unwrap())).await;
    let response = app
        .request(Method::GET, "/v1/system")
        .header("X-Remote-User-Id", "visitor")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let system: System = response.json().await.unwrap();
    assert_eq!(system.credential_role, "user");
    let response = app
        .request(Method::GET, "/v1/shipments")
        .header("X-Remote-User-Id", "visitor")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    for path in ["/v1/agents", "/v1/installation"] {
        assert_eq!(
            app.request(Method::GET, path)
                .header("X-Remote-User-Id", "visitor")
                .header("X-Shipments-Admin", "true")
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::FORBIDDEN
        );
    }
    assert_eq!(
        app.request(Method::POST, "/v1/agents")
            .header("X-Remote-User-Id", "visitor")
            .header("X-Shipments-Request", "1")
            .json(&CreateAgent {
                name: "Forbidden".into()
            })
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    app.close().await;
}
