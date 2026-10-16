//! Exercise recipient-link admission and routing through the public HTTP API.

use std::sync::{Arc, Mutex};

use dropshot::{HttpServer, ResultsPage};
use reqwest::{Client, Method, Response, StatusCode};
use serde::Deserialize;
use shipments::{
    api,
    carrier::Carrier,
    models::{
        CarrierTracking, CreateShipment, Direction, InputTracking, Patch, RegistrationState,
        Shipment, Status, Summary, UpdateShipment,
    },
    shipping_api::{
        PreparedRegistration, ShippingApi,
        error::{Error as ProviderError, ErrorKind},
        types::{ApiFuture, ProviderId, Registration, Tracker},
    },
    store::Store,
    sync::SyncWorker,
};
use tempfile::TempDir;
use uuid::Uuid;

const OWNER: &str = "recipient-test-owner-token-at-least-32-bytes";
const NUMBER: &str = "DOORDASH123456";
const LINK: &str = "https://www.doordash.com/orders/drive?urlCode=first-private-recipient-code";
const OTHER_LINK: &str =
    "https://www.doordash.com/orders/drive?urlCode=second-private-recipient-code";

#[derive(Clone, Debug, PartialEq, Eq)]
struct Call {
    provider: ProviderId,
    operation: &'static str,
    recipient_url: Option<String>,
}

#[derive(Clone, Copy, Default)]
enum Reply {
    #[default]
    Accepted,
    Delivered,
    WrongNumber,
    WrongCarrier,
    Unavailable,
}

#[derive(Clone)]
struct ContractApi {
    provider: ProviderId,
    calls: Arc<Mutex<Vec<Call>>>,
    reply: Arc<Mutex<Reply>>,
    trackers: Arc<Mutex<Vec<(String, Registration)>>>,
}

impl ContractApi {
    fn new(provider: ProviderId, calls: Arc<Mutex<Vec<Call>>>) -> Self {
        Self {
            provider,
            calls,
            reply: Arc::new(Mutex::new(Reply::Accepted)),
            trackers: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn observation(
        &self,
        id: String,
        registration: &Registration,
    ) -> Result<Tracker, ProviderError> {
        let reply = *self.reply.lock().unwrap();
        if matches!(reply, Reply::Unavailable) {
            return Err(ProviderError::new(
                self.provider,
                ErrorKind::Http {
                    status: 503,
                    retry_after: None,
                },
            ));
        }
        let status = if matches!(reply, Reply::Delivered) {
            Status::Delivered
        } else {
            Status::OutForDelivery
        };
        Ok(Tracker {
            provider: self.provider,
            id,
            tracking_code: if matches!(reply, Reply::WrongNumber) {
                "DIFFERENT123456".into()
            } else {
                registration.tracking_number.clone()
            },
            carrier: if matches!(reply, Reply::WrongCarrier) {
                Carrier::Usps
            } else {
                registration.carrier.unwrap_or(Carrier::Usps)
            },
            status,
            public_url: registration.recipient_url.clone(),
            carrier_tracking: CarrierTracking {
                estimated_delivery_at: Some("2026-10-15T19:00:00Z".into()),
                delivered_at: (status == Status::Delivered).then(|| "2026-10-15T20:00:00Z".into()),
                ..Default::default()
            },
        })
    }
}

impl ShippingApi for ContractApi {
    fn id(&self) -> ProviderId {
        self.provider
    }

    fn blocked(&self) -> bool {
        false
    }

    fn for_request(&self, _: &str, _: Option<&str>) -> Arc<dyn ShippingApi> {
        Arc::new(self.clone())
    }

    fn check_connection(&self) -> ApiFuture<'_, ()> {
        Box::pin(async { Ok(()) })
    }

    fn reconcile_registration<'a>(
        &'a self,
        registration: &'a Registration,
    ) -> ApiFuture<'a, Option<Tracker>> {
        Box::pin(async move {
            self.calls.lock().unwrap().push(Call {
                provider: self.provider,
                operation: "verify",
                recipient_url: registration.recipient_url.clone(),
            });
            let id = Uuid::now_v7().to_string();
            let tracker = self.observation(id.clone(), registration)?;
            self.trackers
                .lock()
                .unwrap()
                .push((id, registration.clone()));
            Ok(Some(tracker))
        })
    }

    fn prepare_registration(
        &self,
        _: Registration,
    ) -> ApiFuture<'_, Box<dyn PreparedRegistration>> {
        Box::pin(async { panic!("A complete read-only observation needs no registration") })
    }

    fn retrieve<'a>(&'a self, id: &'a str, number: &'a str) -> ApiFuture<'a, Tracker> {
        Box::pin(async move {
            let registration = self
                .trackers
                .lock()
                .unwrap()
                .iter()
                .find(|(tracker, _)| tracker == id)
                .unwrap()
                .1
                .clone();
            assert_eq!(registration.tracking_number, number);
            self.calls.lock().unwrap().push(Call {
                provider: self.provider,
                operation: "retrieve",
                recipient_url: registration.recipient_url.clone(),
            });
            self.observation(id.into(), &registration)
        })
    }
}

struct App {
    directory: TempDir,
    store: Store,
    server: HttpServer<Arc<api::Context>>,
    providers: Vec<ContractApi>,
    calls: Arc<Mutex<Vec<Call>>>,
    url: String,
    client: Client,
}

impl App {
    async fn new() -> Self {
        Self::with_providers(&[
            ProviderId::EasyPost,
            ProviderId::DoorDash,
            ProviderId::AfterShip,
        ])
        .await
    }

    async fn with_providers(ids: &[ProviderId]) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(&directory.path().join("shipments.sqlite3"))
            .await
            .unwrap();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let providers: Vec<_> = ids
            .iter()
            .copied()
            .map(|id| ContractApi::new(id, calls.clone()))
            .collect();
        let mut context = api::Context::new(store.clone(), OWNER, None, None);
        for provider in &providers {
            context = context.with_provider(Arc::new(provider.clone()));
        }
        let server = api::start("127.0.0.1:0".parse().unwrap(), Arc::new(context)).unwrap();
        let url = format!("http://{}", server.local_addr());
        Self {
            directory,
            store,
            server,
            providers,
            calls,
            url,
            client: Client::new(),
        }
    }

    fn request(&self, method: Method, path: &str) -> reqwest::RequestBuilder {
        self.client
            .request(method, format!("{}{path}", self.url))
            .bearer_auth(OWNER)
    }

    async fn create(&self, input: &CreateShipment, key: &str) -> Response {
        self.request(Method::POST, "/v1/shipments")
            .header("Idempotency-Key", key)
            .json(input)
            .send()
            .await
            .unwrap()
    }

    fn provider(&self, id: ProviderId) -> &ContractApi {
        self.providers.iter().find(|api| api.id() == id).unwrap()
    }

    async fn patch(&self, shipment: &Shipment, tracking: InputTracking) -> Response {
        self.request(Method::PATCH, &format!("/v1/shipments/{}", shipment.id))
            .header("If-Match", format!("\"{}\"", shipment.revision))
            .json(&UpdateShipment {
                tracking: Patch {
                    value: Some(Some(tracking)),
                },
                ..Default::default()
            })
            .send()
            .await
            .unwrap()
    }

    async fn close(self) {
        self.server.close().await.unwrap();
        self.store.close().await.unwrap();
    }
}

fn input(carrier: Carrier, recipient_url: Option<&str>) -> CreateShipment {
    CreateShipment {
        title: "Recipient tracking test parcel".into(),
        direction: Direction::Inbound,
        merchant: None,
        order_reference: None,
        signature_required: None,
        source: None,
        tracking: InputTracking {
            carrier: Some(carrier),
            number: NUMBER.into(),
            recipient_url: recipient_url.map(str::to_owned),
        },
    }
}

#[derive(Deserialize)]
struct Failure {
    error_code: String,
    message: String,
}

#[derive(Deserialize)]
struct WireShipment {
    sync: WireSync,
}

#[derive(Deserialize)]
struct WireSync {
    provider: String,
}

#[derive(Deserialize)]
struct WireSystem {
    enabled_providers: Vec<String>,
}

#[tokio::test]
async fn doordash_requires_a_valid_original_recipient_link_before_any_provider_io() {
    let app = App::new().await;
    for (index, url) in [
        None,
        Some(""),
        Some("  "),
        Some("http://www.doordash.com/orders/drive?urlCode=private-invalid-code"),
        Some("https://example.com/orders/drive?urlCode=private-invalid-code"),
        Some("https://www.doordash.com.evil.example/orders/drive?urlCode=private-invalid-code"),
        Some("https://www.doordash.com/orders/drive"),
        Some("https://www.doordash.com/orders/drive?urlCode="),
        Some("https://user:password@www.doordash.com/orders/drive?urlCode=private-invalid-code"),
    ]
    .into_iter()
    .enumerate()
    {
        let response = app
            .create(&input(Carrier::DoorDash, url), &format!("invalid-{index}"))
            .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{url:?}");
        let failure: Failure = response.json().await.unwrap();
        assert!(failure.message.contains("DoorDash"));
        assert!(failure.message.to_lowercase().contains("link"));
        assert!(matches!(
            failure.error_code.as_str(),
            "tracking_recipient_url_required" | "tracking_recipient_url_invalid"
        ));
        assert!(!failure.message.contains("private-invalid-code"));
        assert!(!failure.message.contains("user:password"));
    }
    // Explicit JSON null and omission follow the same public validation path.
    for body in [
        r#"{"title":"Parcel","direction":"inbound","tracking":{"carrier":"DoorDash","number":"DOORDASH123456"}}"#,
        r#"{"title":"Parcel","direction":"inbound","tracking":{"carrier":"DoorDash","number":"DOORDASH123456","recipient_url":null}}"#,
    ] {
        let response = app
            .request(Method::POST, "/v1/shipments")
            .header("Idempotency-Key", Uuid::now_v7().to_string())
            .header("Content-Type", "application/json")
            .body(body)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }
    assert!(app.calls.lock().unwrap().is_empty());
    assert_eq!(app.store.summary(None).await.unwrap().active, 0);
    app.close().await;
}

#[tokio::test]
async fn original_recipient_link_resolves_doordash_but_cannot_be_used_for_another_carrier() {
    let app = App::new().await;
    let mut body = input(Carrier::DoorDash, Some(LINK));
    body.tracking.carrier = None;
    let response = app.create(&body, "inferred").await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let created: Shipment = response.json().await.unwrap();
    assert_eq!(created.tracking.carrier, Carrier::DoorDash);
    assert_eq!(created.sync.provider, ProviderId::DoorDash);
    let rejected = app
        .create(&input(Carrier::Usps, Some(LINK)), "mismatch")
        .await;
    assert_eq!(rejected.status(), StatusCode::BAD_REQUEST);
    let failure: Failure = rejected.json().await.unwrap();
    assert_eq!(failure.error_code, "tracking_recipient_url_invalid");
    assert!(!failure.message.contains("first-private-recipient-code"));
    assert_eq!(app.calls.lock().unwrap().len(), 1);
    app.close().await;
}

#[tokio::test]
async fn doordash_admission_saves_and_reloads_the_verified_recipient_link() {
    let app = App::new().await;
    let response = app
        .create(&input(Carrier::DoorDash, Some(LINK)), "new")
        .await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let created: Shipment = response.json().await.unwrap();
    assert_eq!(created.tracking.recipient_url.as_deref(), Some(LINK));
    assert_eq!(created.sync.provider, ProviderId::DoorDash);
    assert_eq!(
        created.sync.registration_state,
        RegistrationState::Registered
    );
    assert_eq!(created.status, Status::OutForDelivery);
    assert_eq!(created.sync.tracking_url.as_deref(), Some(LINK));
    assert_eq!(
        app.calls.lock().unwrap().as_slice(),
        [Call {
            provider: ProviderId::DoorDash,
            operation: "verify",
            recipient_url: Some(LINK.into()),
        }]
    );
    let reloaded: Shipment = app
        .request(Method::GET, &format!("/v1/shipments/{}", created.id))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(reloaded.tracking, created.tracking);
    let reopened = Store::open(&app.directory.path().join("shipments.sqlite3"))
        .await
        .unwrap();
    assert_eq!(
        reopened.get(created.id).await.unwrap().tracking,
        created.tracking
    );
    reopened.close().await.unwrap();
    *app.provider(ProviderId::DoorDash).reply.lock().unwrap() = Reply::Delivered;
    sqlx::query("UPDATE shipments SET next_attempt_at=0,lease_until=0 WHERE id=?")
        .bind(created.id.to_string())
        .execute(app.store.pool())
        .await
        .unwrap();
    assert!(
        SyncWorker::new(
            app.store.pool().clone(),
            Arc::new(app.provider(ProviderId::DoorDash).clone()),
        )
        .tick()
        .await
        .unwrap()
    );
    let delivered = app.store.get(created.id).await.unwrap();
    assert_eq!(delivered.status, Status::Delivered);
    assert_eq!(
        delivered.carrier_tracking.delivered_at.as_deref(),
        Some("2026-10-15T20:00:00Z")
    );
    let summary: Summary = app
        .request(Method::GET, "/v1/summary")
        .query(&[
            ("direction", "inbound"),
            ("delivered_after", "1792047600"),
            ("delivered_before", "1792134000"),
        ])
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(summary.delivered, 1);
    assert_eq!(app.calls.lock().unwrap().len(), 2);
    app.close().await;
}

#[tokio::test]
async fn rejected_or_uncertain_doordash_observations_never_create_a_shipment() {
    for reply in [Reply::WrongNumber, Reply::WrongCarrier, Reply::Unavailable] {
        let app = App::new().await;
        *app.provider(ProviderId::DoorDash).reply.lock().unwrap() = reply;
        let response = app
            .create(&input(Carrier::DoorDash, Some(LINK)), "unverified")
            .await;
        assert!(!response.status().is_success());
        let failure: Failure = response.json().await.unwrap();
        assert!(!failure.message.contains("first-private-recipient-code"));
        assert!(!failure.message.contains(LINK));
        assert_eq!(app.store.summary(None).await.unwrap().active, 0);
        let page: ResultsPage<Shipment> = app
            .request(Method::GET, "/v1/shipments")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert!(page.items.is_empty());
        assert!(
            app.calls
                .lock()
                .unwrap()
                .iter()
                .all(|call| call.provider == ProviderId::DoorDash)
        );
        app.close().await;
    }
}

#[tokio::test]
async fn doordash_link_cannot_be_cleared_and_changed_link_is_verified_before_commit() {
    let app = App::new().await;
    let created: Shipment = app
        .create(&input(Carrier::DoorDash, Some(LINK)), "original")
        .await
        .json()
        .await
        .unwrap();
    for url in [
        None,
        Some(""),
        Some("https://example.com/private-invalid-code"),
    ] {
        let response = app
            .patch(&created, input(Carrier::DoorDash, url).tracking)
            .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let preserved = app.store.get(created.id).await.unwrap();
        assert_eq!(preserved.tracking, created.tracking);
        assert_eq!(preserved.sync.tracker_id, created.sync.tracker_id);
        assert_eq!(preserved.revision, created.revision);
    }
    assert_eq!(app.calls.lock().unwrap().len(), 1);
    let response = app
        .patch(
            &created,
            input(Carrier::DoorDash, Some(OTHER_LINK)).tracking,
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let changed: Shipment = response.json().await.unwrap();
    assert_eq!(changed.tracking.recipient_url.as_deref(), Some(OTHER_LINK));
    assert_eq!(changed.sync.tracking_url.as_deref(), Some(OTHER_LINK));
    assert_ne!(changed.sync.tracker_id, created.sync.tracker_id);
    assert_eq!(changed.sync.provider, ProviderId::DoorDash);
    assert_eq!(app.calls.lock().unwrap().len(), 2);
    assert_eq!(
        app.calls.lock().unwrap()[1].recipient_url.as_deref(),
        Some(OTHER_LINK)
    );
    let generation: i64 =
        sqlx::query_scalar("SELECT tracking_generation FROM shipments WHERE id=?")
            .bind(created.id.to_string())
            .fetch_one(app.store.pool())
            .await
            .unwrap();
    assert_eq!(generation, 1);
    app.close().await;
}

#[tokio::test]
async fn correcting_a_doordash_link_requires_a_new_idempotency_key() {
    let app = App::new().await;
    let original = input(Carrier::DoorDash, Some(LINK));
    let created: Shipment = app
        .create(&original, "same-key")
        .await
        .json()
        .await
        .unwrap();
    let replay = app.create(&original, "same-key").await;
    assert_eq!(replay.status(), StatusCode::CREATED);
    assert_eq!(replay.json::<Shipment>().await.unwrap().id, created.id);
    let changed = app
        .create(&input(Carrier::DoorDash, Some(OTHER_LINK)), "same-key")
        .await;
    assert_eq!(changed.status(), StatusCode::CONFLICT);
    assert_eq!(app.calls.lock().unwrap().len(), 1);
    assert_eq!(
        app.store.get(created.id).await.unwrap().tracking,
        created.tracking
    );
    app.close().await;
}

#[tokio::test]
async fn uncertain_doordash_admission_keeps_the_key_bound_to_its_original_link() {
    let app = App::new().await;
    *app.provider(ProviderId::DoorDash).reply.lock().unwrap() = Reply::Unavailable;
    let original = input(Carrier::DoorDash, Some(LINK));
    let response = app.create(&original, "failed-key").await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(app.store.summary(None).await.unwrap().active, 0);
    let changed = app
        .create(&input(Carrier::DoorDash, Some(OTHER_LINK)), "failed-key")
        .await;
    assert_eq!(changed.status(), StatusCode::CONFLICT);
    assert_eq!(app.calls.lock().unwrap().len(), 1);
    app.close().await;
}

#[tokio::test]
async fn failed_doordash_link_change_preserves_identity_and_retries_the_new_link() {
    let app = App::new().await;
    let created: Shipment = app
        .create(&input(Carrier::DoorDash, Some(LINK)), "original")
        .await
        .json()
        .await
        .unwrap();
    *app.provider(ProviderId::DoorDash).reply.lock().unwrap() = Reply::Unavailable;
    let response = app
        .patch(
            &created,
            input(Carrier::DoorDash, Some(OTHER_LINK)).tracking,
        )
        .await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let preserved = app.store.get(created.id).await.unwrap();
    assert_eq!(preserved.tracking, created.tracking);
    assert_eq!(preserved.revision, created.revision);
    assert_eq!(preserved.sync.tracker_id, created.sync.tracker_id);
    assert_eq!(app.calls.lock().unwrap().len(), 2);
    // The failed attempt now names the new URL while the successful cached
    // observation still belongs to the old one. Expiring backoff must not make
    // that old observation eligible to satisfy the corrected link.
    sqlx::query("UPDATE tracking_verifications SET next_attempt_at=0 WHERE provider='doordash' AND tracking_number=?")
        .bind(NUMBER).execute(app.store.pool()).await.unwrap();
    *app.provider(ProviderId::DoorDash).reply.lock().unwrap() = Reply::Accepted;
    let retried = app
        .patch(
            &created,
            input(Carrier::DoorDash, Some(OTHER_LINK)).tracking,
        )
        .await;
    assert_eq!(retried.status(), StatusCode::OK);
    let recovered: Shipment = retried.json().await.unwrap();
    assert_eq!(
        recovered.tracking.recipient_url.as_deref(),
        Some(OTHER_LINK)
    );
    assert_eq!(recovered.sync.tracking_url.as_deref(), Some(OTHER_LINK));
    assert_ne!(recovered.sync.tracker_id, created.sync.tracker_id);
    let calls = app.calls.lock().unwrap().clone();
    assert_eq!(calls.len(), 3);
    assert_eq!(calls[2].operation, "verify");
    assert_eq!(calls[2].recipient_url.as_deref(), Some(OTHER_LINK));
    app.close().await;
}

#[tokio::test]
async fn carrier_routing_never_falls_back_to_a_different_tracking_provider() {
    let app = App::new().await;
    for (index, carrier, provider, wire_name) in [
        (0, Carrier::DoorDash, ProviderId::DoorDash, "doordash"),
        (1, Carrier::SpeedX, ProviderId::AfterShip, "aftership"),
        (2, Carrier::Sway, ProviderId::AfterShip, "aftership"),
        (3, Carrier::Veho, ProviderId::AfterShip, "aftership"),
        (4, Carrier::Usps, ProviderId::EasyPost, "easypost"),
    ] {
        let mut body = input(carrier, (carrier == Carrier::DoorDash).then_some(LINK));
        body.tracking.number = format!("ROUTING{index}123456");
        let response = app.create(&body, &format!("route-{index}")).await;
        assert_eq!(response.status(), StatusCode::CREATED);
        let bytes = response.bytes().await.unwrap();
        assert_eq!(
            serde_json::from_slice::<Shipment>(&bytes)
                .unwrap()
                .sync
                .provider,
            provider
        );
        assert_eq!(
            serde_json::from_slice::<WireShipment>(&bytes)
                .unwrap()
                .sync
                .provider,
            wire_name
        );
        assert_eq!(app.calls.lock().unwrap()[index].provider, provider);
    }
    let mut detected = input(Carrier::Usps, None);
    detected.tracking.carrier = None;
    detected.tracking.number = "ROUTING5123456".into();
    let response = app.create(&detected, "autodetect").await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let resolved: Shipment = response.json().await.unwrap();
    assert_eq!(resolved.tracking.carrier, Carrier::Usps);
    assert_eq!(resolved.sync.provider, ProviderId::EasyPost);
    assert_eq!(app.calls.lock().unwrap()[5].provider, ProviderId::EasyPost);
    assert_eq!(app.calls.lock().unwrap().len(), 6);
    let mut system: WireSystem = app
        .request(Method::GET, "/v1/system")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    system.enabled_providers.sort();
    assert_eq!(
        system.enabled_providers,
        ["aftership", "doordash", "easypost"]
    );
    app.close().await;
}

#[tokio::test]
async fn changing_tracking_to_veho_verifies_and_saves_aftership_ownership() {
    let app = App::new().await;
    let created: Shipment = app
        .create(&input(Carrier::Usps, None), "before-veho")
        .await
        .json()
        .await
        .unwrap();
    let response = app
        .patch(&created, input(Carrier::Veho, None).tracking)
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let updated: Shipment = response.json().await.unwrap();
    assert_eq!(updated.tracking.carrier, Carrier::Veho);
    assert_eq!(updated.sync.provider, ProviderId::AfterShip);
    assert_eq!(
        updated.sync.registration_state,
        RegistrationState::Registered
    );
    assert_ne!(updated.sync.tracker_id, created.sync.tracker_id);
    let calls = app.calls.lock().unwrap().clone();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].provider, ProviderId::EasyPost);
    assert_eq!(calls[1].provider, ProviderId::AfterShip);
    assert_eq!(calls[1].operation, "verify");
    let durable = app.store.get(updated.id).await.unwrap();
    assert_eq!(durable.sync.provider, ProviderId::AfterShip);
    assert_eq!(durable.sync.tracker_id, updated.sync.tracker_id);
    app.close().await;
}

#[tokio::test]
async fn missing_required_provider_is_actionable_and_never_falls_back_to_easypost() {
    for carrier in [
        Carrier::DoorDash,
        Carrier::SpeedX,
        Carrier::Sway,
        Carrier::Veho,
    ] {
        let app = App::with_providers(&[ProviderId::EasyPost]).await;
        let response = app
            .create(
                &input(carrier, (carrier == Carrier::DoorDash).then_some(LINK)),
                "missing-provider",
            )
            .await;
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let failure: Failure = response.json().await.unwrap();
        let provider = if carrier == Carrier::DoorDash {
            "DoorDash"
        } else {
            "AfterShip"
        };
        assert!(failure.message.contains(provider));
        assert!(!failure.message.contains(LINK));
        assert!(app.calls.lock().unwrap().is_empty());
        assert_eq!(app.store.summary(None).await.unwrap().active, 0);
        app.close().await;
    }
}

#[tokio::test]
async fn replacing_a_provider_replaces_its_calls_instead_of_adding_a_second_route() {
    let app = App::with_providers(&[ProviderId::EasyPost, ProviderId::EasyPost]).await;
    *app.providers[0].reply.lock().unwrap() = Reply::Unavailable;
    let response = app.create(&input(Carrier::Usps, None), "replacement").await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let created: Shipment = response.json().await.unwrap();
    assert_eq!(created.sync.provider, ProviderId::EasyPost);
    assert_eq!(app.calls.lock().unwrap().len(), 1);
    app.close().await;
}
