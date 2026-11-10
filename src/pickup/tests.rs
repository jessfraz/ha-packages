//! Public carrier contracts exercised over real HTTP with fictional credentials.
use crate::pickup::{models::*, *};
use serde::Deserialize;
use std::{
    collections::HashMap,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
};

pub(crate) struct Request {
    pub method: String,
    pub path: String,
    pub headers: HashMap<String, String>,
    pub body: Vec<u8>,
}
pub(crate) struct Reply {
    pub status: u16,
    pub headers: String,
    pub body: String,
}
impl Reply {
    pub fn ok(body: &str) -> Self {
        Self {
            status: 200,
            headers: "".into(),
            body: body.into(),
        }
    }
}
pub(crate) struct Fixture {
    pub origin: String,
    stop: Arc<AtomicBool>,
    address: std::net::SocketAddr,
    handle: Option<thread::JoinHandle<()>>,
}
impl Fixture {
    pub fn new(handler: impl Fn(Request) -> Option<Reply> + Send + 'static) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let handle = thread::spawn(move || {
            for stream in listener.incoming() {
                let mut stream = stream.unwrap();
                if stopped.load(Ordering::Acquire) {
                    break;
                }
                stream
                    .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                    .unwrap();
                let mut head = Vec::new();
                while !head.ends_with(b"\r\n\r\n") {
                    let mut byte = [0];
                    stream.read_exact(&mut byte).unwrap();
                    head.push(byte[0]);
                    assert!(head.len() < 32768);
                }
                let head = String::from_utf8(head).unwrap();
                let mut lines = head.split("\r\n");
                let mut first = lines.next().unwrap().split_whitespace();
                let method = first.next().unwrap().into();
                let path = first.next().unwrap().into();
                let headers: HashMap<_, _> = lines
                    .filter_map(|s| s.split_once(':'))
                    .map(|(k, v)| (k.to_ascii_lowercase(), v.trim().to_string()))
                    .collect();
                let length = headers
                    .get("content-length")
                    .map_or(0, |s| s.parse().unwrap());
                let mut body = vec![0; length];
                stream.read_exact(&mut body).unwrap();
                if let Some(reply) = handler(Request {
                    method,
                    path,
                    headers,
                    body,
                }) {
                    write!(stream, "HTTP/1.1 {} Fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n{}\r\n{}", reply.status, reply.body.len(), reply.headers, reply.body).unwrap();
                }
            }
        });
        Self {
            origin: format!("http://{address}"),
            stop,
            address,
            handle: Some(handle),
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        let _ = TcpStream::connect(self.address);
        self.handle.take().unwrap().join().unwrap();
    }
}
fn credentials() -> Credentials {
    Credentials {
        client_id: "fixture-client".into(),
        client_secret: "fixture-secret".into(),
        environment: Environment::Sandbox,
    }
}
fn account(number: &str) -> AccountCredentials {
    AccountCredentials {
        client_id: "fixture-client".into(),
        client_secret: "fixture-secret".into(),
        environment: Environment::Sandbox,
        account_number: number.into(),
    }
}
pub(crate) fn site_input() -> SiteInput {
    SiteInput {
        name: "Example site".into(),
        contact_name: "Example Person".into(),
        company: None,
        phone: "2025550100".into(),
        email: Some("example@example.invalid".into()),
        address: Address {
            street: "123 Example Street".into(),
            secondary: None,
            city: "Example City".into(),
            state: "CA".into(),
            postal_code: "90210".into(),
            country: "US".into(),
        },
        timezone: "America/Los_Angeles".into(),
        ready_time: "09:00".into(),
        close_time: "18:00".into(),
        pickup_point: "FRONT_DOOR".into(),
        instructions: None,
        residential: true,
    }
}
fn plan(service: PickupService) -> ResolvedPlan {
    ResolvedPlan {
        reference: uuid::Uuid::now_v7(),
        carrier: service.carrier(),
        site: Site {
            id: uuid::Uuid::now_v7(),
            revision: 1,
            is_default: true,
            details: site_input(),
        },
        date: Some("2027-02-02".into()),
        next_available: false,
        ready_time: "09:00".into(),
        close_time: "18:00".into(),
        parcels: vec![ParcelGroup {
            service,
            count: 1,
            destination_country: "US".into(),
        }],
        total_weight_hundredths: 250,
        overweight: false,
        tracking: vec!["TEST000001".into()],
    }
}
fn token(request: &Request, path: &str) -> Option<Reply> {
    if request.path != path {
        return None;
    }
    assert_eq!(request.method, "POST");
    Some(Reply::ok(
        r#"{"access_token":"fixture-access","expires_in":"3600"}"#,
    ))
}
#[tokio::test]
async fn usps_uses_normal_route_contract_and_conditional_cancellation() {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Create {
        pickup_date: String,
        next_available_pickup: bool,
        estimated_weight: f64,
        packages: Vec<Package>,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Package {
        package_type: String,
        package_count: u16,
    }
    let calls = Arc::new(Mutex::new(Vec::new()));
    let seen = calls.clone();
    let fixture = Fixture::new(move |r| {
        seen.lock()
            .unwrap()
            .push((r.method.clone(), r.path.clone()));
        if let Some(reply) = token(&r, "/oauth2/v3/token") {
            #[derive(Deserialize)]
            struct Auth {
                grant_type: String,
                client_id: String,
                client_secret: String,
            }
            let auth: Auth = serde_json::from_slice(&r.body).unwrap();
            assert_eq!(auth.grant_type, "client_credentials");
            assert_eq!(auth.client_id, "fixture-client");
            assert_eq!(auth.client_secret, "fixture-secret");
            return Some(reply);
        }
        assert_eq!(r.headers["authorization"], "Bearer fixture-access");
        if r.path.starts_with("/pickup/v3/carrier-pickup/eligibility?") {
            assert_eq!(r.method, "GET");
            return Some(Reply::ok(
                r#"{"address":{"streetAddress":"123 Example Street","city":"Example City","state":"CA","ZIPCode":"90210"}}"#,
            ));
        }
        if r.method == "POST" {
            assert_eq!(r.path, "/pickup/v3/carrier-pickup");
            let body: Create = serde_json::from_slice(&r.body).unwrap();
            assert_eq!(body.pickup_date, "2027-02-02");
            assert!(!body.next_available_pickup);
            assert_eq!(body.estimated_weight, 2.5);
            assert_eq!(body.packages[0].package_type, "RETURNS");
            assert_eq!(body.packages[0].package_count, 1);
            return Some(Reply::ok(
                r#"{"confirmationNumber":"USPS-FIXTURE","pickupDate":"2027-02-02"}"#,
            ));
        }
        assert_eq!(r.path, "/pickup/v3/carrier-pickup/USPS-FIXTURE");
        if r.method == "GET" {
            Some(Reply {
                status: 200,
                headers: "ETag: \"carrier-2\"\r\n".into(),
                body: r#"[{"confirmationNumber":"USPS-FIXTURE","pickupDate":"2027-02-02"}]"#.into(),
            })
        } else {
            assert_eq!(r.method, "DELETE");
            assert_eq!(r.headers["if-match"], "\"carrier-2\"");
            Some(Reply::ok("{}"))
        }
    });
    let client = usps::Usps::new(credentials())
        .unwrap()
        .with_origin(fixture.origin.clone());
    let plan = plan(PickupService::UspsReturns);
    assert_eq!(client.preview(&plan).await.unwrap().fee, Fee::Free);
    let receipt = client.schedule(&plan).await.unwrap();
    client.cancel(&plan, &receipt).await.unwrap();
    assert_eq!(calls.lock().unwrap().len(), 5);
}
#[tokio::test]
async fn ups_quotes_and_bills_shipping_account_then_uses_header_based_status_and_cancel() {
    #[derive(Deserialize)]
    #[serde(rename_all = "PascalCase")]
    struct Envelope {
        pickup_creation_request: Create,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "PascalCase")]
    struct Create {
        payment_method: String,
        shipper: Shipper,
        pickup_piece: Vec<Piece>,
        reference_number: String,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "PascalCase")]
    struct Shipper {
        account: Account,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "PascalCase")]
    struct Account {
        account_number: String,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "PascalCase")]
    struct Piece {
        service_code: String,
        quantity: String,
    }
    let reference = plan(PickupService::UpsGround);
    let expected = reference.reference.simple().to_string();
    let record_reference = expected.clone();
    // UPS Pickup.yaml declares /api in both REST server URLs. OAuth is
    // independently host-level, so assert the actual paths received on wire.
    let paths = Arc::new(Mutex::new(Vec::new()));
    let observed = paths.clone();
    let fixture = Fixture::new(move |r| {
        observed.lock().unwrap().push(r.path.clone());
        if let Some(reply) = token(&r, "/security/v1/oauth/token") {
            assert!(r.headers["authorization"].starts_with("Basic "));
            assert_eq!(r.body, b"grant_type=client_credentials");
            return Some(reply);
        }
        assert_eq!(r.headers["authorization"], "Bearer fixture-access");
        match (r.method.as_str(), r.path.as_str()) {
            ("POST", "/api/shipments/v2409/pickup/oncall") => Some(Reply::ok(
                r#"{"PickupRateResponse":{"Response":{"ResponseStatus":{"Code":"1"}},"RateResult":{"CurrencyCode":"USD","GrandTotalOfAllCharge":"8.50"}}}"#,
            )),
            ("POST", "/api/pickupcreation/v2409/pickup") => {
                let body: Envelope = serde_json::from_slice(&r.body).unwrap();
                let body = body.pickup_creation_request;
                assert_eq!(body.payment_method, "01");
                assert_eq!(body.shipper.account.account_number, "ABC123");
                assert_eq!(body.pickup_piece[0].service_code, "003");
                assert_eq!(body.pickup_piece[0].quantity, "1");
                assert_eq!(body.reference_number, record_reference);
                Some(Reply::ok(
                    r#"{"PickupCreationResponse":{"Response":{"ResponseStatus":{"Code":"1"}},"PRN":"UPS-FIXTURE"}}"#,
                ))
            }
            ("GET", "/api/shipments/v2409/pickup/oncall") => {
                assert_eq!(r.headers["accountnumber"], "ABC123");
                Some(Reply::ok(&format!(
                    r#"{{"PickupPendingStatusResponse":{{"Response":{{"ResponseStatus":{{"Code":"1"}}}},"PendingStatus":[{{"PRN":"UPS-FIXTURE","ServiceDate":"20270202","ReferenceNumber":"{expected}"}}]}}}}"#
                )))
            }
            ("DELETE", "/api/shipments/v2409/pickup/02") => {
                assert_eq!(r.headers["prn"], "UPS-FIXTURE");
                assert!(r.body.is_empty());
                Some(Reply::ok(
                    r#"{"PickupCancelResponse":{"Response":{"ResponseStatus":{"Code":"1"}}}}"#,
                ))
            }
            _ => Some(Reply {
                status: 404,
                headers: String::new(),
                body: format!("Unrecognized official UPS REST path: {}", r.path),
            }),
        }
    });
    let client = ups::Ups::new(account("ABC123"))
        .unwrap()
        .with_origin(fixture.origin.clone());
    let preview = client.preview(&reference).await;
    assert_eq!(
        &paths.lock().unwrap()[..],
        &[
            "/security/v1/oauth/token",
            "/api/shipments/v2409/pickup/oncall"
        ]
    );
    assert_eq!(
        preview.unwrap().fee,
        Fee::Estimate {
            currency: "USD".into(),
            amount: "8.50".into()
        }
    );
    let receipt = client.schedule(&reference).await.unwrap();
    assert_eq!(
        client.lookup(&reference, &receipt).await.unwrap().number,
        "UPS-FIXTURE"
    );
    client.cancel(&reference, &receipt).await.unwrap();
}
#[tokio::test]
async fn fedex_requires_real_service_and_bills_account_without_inventing_a_status_api() {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Create {
        carrier_code: String,
        associated_account_number: Account,
        pickup_charges_payment: Payment,
    }
    #[derive(Deserialize)]
    struct Account {
        value: String,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Payment {
        payment_type: String,
        payor_type: String,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Availability {
        shipment_attributes: Attributes,
        carriers: Vec<String>,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Attributes {
        service_type: String,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Cancel {
        scheduled_date: String,
        location: String,
        pickup_confirmation_code: String,
    }
    let fixture = Fixture::new(move |r| {
        if let Some(reply) = token(&r, "/oauth/token") {
            assert!(String::from_utf8_lossy(&r.body).contains("client_secret=fixture-secret"));
            return Some(reply);
        }
        match (r.method.as_str(), r.path.as_str()) {
            ("POST", "/pickup/v1/pickups/availabilities") => {
                let body: Availability = serde_json::from_slice(&r.body).unwrap();
                assert_eq!(body.shipment_attributes.service_type, "PRIORITY_OVERNIGHT");
                assert_eq!(body.carriers, ["FDXE"]);
                Some(Reply::ok(
                    r#"{"output":{"options":[{"carrier":"FDXE","available":true,"pickupDate":"2027-02-02","cutOffTime":"16:00:00","accessTime":{"hours":2,"minutes":0},"residentialAvailable":true}]}}"#,
                ))
            }
            ("POST", "/pickup/v1/pickups") => {
                let body: Create = serde_json::from_slice(&r.body).unwrap();
                assert_eq!(body.carrier_code, "FDXE");
                assert_eq!(body.associated_account_number.value, "123456789");
                assert_eq!(body.pickup_charges_payment.payment_type, "ACCOUNT");
                assert_eq!(body.pickup_charges_payment.payor_type, "SENDER");
                Some(Reply::ok(
                    r#"{"output":{"pickupConfirmationCode":"FDX-FIXTURE","location":"TEST"}}"#,
                ))
            }
            ("PUT", "/pickup/v1/pickups/cancel") => {
                let body: Cancel = serde_json::from_slice(&r.body).unwrap();
                assert_eq!(body.scheduled_date, "2027-02-02");
                assert_eq!(body.location, "TEST");
                assert_eq!(body.pickup_confirmation_code, "FDX-FIXTURE");
                Some(Reply::ok(
                    r#"{"output":{"pickupConfirmationCode":"FDX-FIXTURE","cancelConfirmationMessage":"Cancelled"}}"#,
                ))
            }
            _ => panic!("unexpected {} {}", r.method, r.path),
        }
    });
    let client = fedex::FedEx::new(account("123456789"))
        .unwrap()
        .with_origin(fixture.origin.clone());
    let plan = plan(PickupService::FedExPriorityOvernight);
    assert_eq!(client.preview(&plan).await.unwrap().fee, Fee::Unquoted);
    let receipt = client.schedule(&plan).await.unwrap();
    assert_eq!(
        client.lookup(&plan, &receipt).await.err().unwrap().code,
        ErrorCode::Unsupported
    );
    client.cancel(&plan, &receipt).await.unwrap();
}
#[tokio::test]
async fn carrier_gate_obeys_retry_after_for_manual_calls_and_never_retries_lost_physical_response()
{
    let count = Arc::new(Mutex::new(0));
    let observed = count.clone();
    let fixture = Fixture::new(move |r| {
        if let Some(reply) = token(&r, "/oauth2/v3/token") {
            return Some(reply);
        }
        *observed.lock().unwrap() += 1;
        Some(Reply {
            status: 429,
            headers: "Retry-After: 60\r\n".into(),
            body: "private-response-detail".into(),
        })
    });
    let client = http::CarrierHttp::new(PickupCarrier::Usps, credentials(), None)
        .unwrap()
        .with_origin(fixture.origin.clone());
    for _ in 0..2 {
        let error = client
            .request::<()>(reqwest::Method::GET, "/test", &[], None, &[], false)
            .await
            .err()
            .unwrap();
        assert_eq!(error.code, ErrorCode::RateLimited);
        assert!(!error.to_string().contains("private"));
    }
    assert_eq!(*count.lock().unwrap(), 1);
    let count = Arc::new(Mutex::new(0));
    let observed = count.clone();
    let fixture = Fixture::new(move |r| {
        if let Some(reply) = token(&r, "/oauth2/v3/token") {
            return Some(reply);
        }
        *observed.lock().unwrap() += 1;
        None
    });
    let client = http::CarrierHttp::new(PickupCarrier::Usps, credentials(), None)
        .unwrap()
        .with_origin(fixture.origin.clone());
    let error = client
        .request::<()>(reqwest::Method::POST, "/test", &[], Some(&()), &[], true)
        .await
        .err()
        .unwrap();
    assert_eq!(error.code, ErrorCode::UnknownOutcome);
    assert_eq!(*count.lock().unwrap(), 1);
}

#[tokio::test]
async fn oauth_and_physical_requests_never_follow_redirects_or_accept_unbounded_bodies() {
    let redirected = Arc::new(Mutex::new(0));
    let seen = redirected.clone();
    let target = Fixture::new(move |_| {
        *seen.lock().unwrap() += 1;
        Some(Reply::ok("{}"))
    });
    let location = target.origin.clone();
    let fixture = Fixture::new(move |r| {
        if let Some(reply) = token(&r, "/oauth2/v3/token") {
            return Some(reply);
        }
        Some(Reply {
            status: 307,
            headers: format!("Location: {location}/stolen\r\n"),
            body: "private detail".into(),
        })
    });
    let client = http::CarrierHttp::new(PickupCarrier::Usps, credentials(), None)
        .unwrap()
        .with_origin(fixture.origin.clone());
    let error = client
        .request::<()>(reqwest::Method::POST, "/test", &[], Some(&()), &[], true)
        .await
        .err()
        .unwrap();
    assert_eq!(error.code, ErrorCode::UnknownOutcome);
    assert_eq!(*redirected.lock().unwrap(), 0);
    let fixture = Fixture::new(move |r| {
        if let Some(reply) = token(&r, "/oauth2/v3/token") {
            return Some(reply);
        }
        Some(Reply {
            status: 200,
            headers: String::new(),
            body: "x".repeat(2 * 1024 * 1024 + 1),
        })
    });
    let client = http::CarrierHttp::new(PickupCarrier::Usps, credentials(), None)
        .unwrap()
        .with_origin(fixture.origin.clone());
    let error = client
        .request::<()>(reqwest::Method::POST, "/test", &[], Some(&()), &[], true)
        .await
        .err()
        .unwrap();
    assert_eq!(error.code, ErrorCode::UnknownOutcome);
}
#[test]
fn billing_binding_survives_secret_rotation_but_not_account_or_environment_changes() {
    let initial = ups::Ups::new(account("ABC123")).unwrap().fingerprint();
    let mut rotated = account("ABC123");
    rotated.client_secret = "rotated-fixture-secret".into();
    assert_eq!(initial, ups::Ups::new(rotated).unwrap().fingerprint());
    assert_ne!(
        initial,
        ups::Ups::new(account("DEF456")).unwrap().fingerprint()
    );
    let mut production = account("ABC123");
    production.environment = Environment::Production;
    assert_ne!(initial, ups::Ups::new(production).unwrap().fingerprint());
}

#[tokio::test]
async fn manual_preview_respects_the_full_provider_pause_after_a_cold_restart() {
    let calls = Arc::new(Mutex::new(0));
    let seen = calls.clone();
    let fixture = Fixture::new(move |r| {
        if let Some(reply) = token(&r, "/security/v1/oauth/token") {
            return Some(reply);
        }
        *seen.lock().unwrap() += 1;
        Some(Reply {
            status: 429,
            headers: "Retry-After: 172800\r\n".into(),
            body: "fixture throttle".into(),
        })
    });
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("packages.sqlite3");
    let store = crate::store::Store::open(&path).await.unwrap();
    let manager = service::Manager::unconfigured(&store).with_provider(Arc::new(
        ups::Ups::new(account("ABC123"))
            .unwrap()
            .with_origin(fixture.origin.clone()),
    ));
    manager
        .save_site(None, None, site_input(), true)
        .await
        .unwrap();
    let request = ScheduleRequest {
        carrier: Some(PickupCarrier::Ups),
        service: Some(PickupService::UpsGround),
        weight_lbs: Some("2.50".into()),
        ..Default::default()
    };
    let error = manager.preview(&request).await.err().unwrap();
    assert_eq!(error.code, ErrorCode::RateLimited);
    assert_eq!(error.retry_after_seconds, Some(172800));
    store.close().await.unwrap();
    let restored = crate::store::Store::open(&path).await.unwrap();
    let manager = service::Manager::unconfigured(&restored).with_provider(Arc::new(
        ups::Ups::new(account("ABC123"))
            .unwrap()
            .with_origin(fixture.origin.clone()),
    ));
    let error = manager.preview(&request).await.err().unwrap();
    assert_eq!(error.code, ErrorCode::RateLimited);
    assert!(error.retry_after_seconds.unwrap() >= 172790);
    assert_eq!(*calls.lock().unwrap(), 1);
    restored.close().await.unwrap();
}

#[tokio::test]
async fn shutdown_drains_a_fenced_real_http_dispatch_and_a_new_key_reuses_its_receipt() {
    let calls = Arc::new(Mutex::new(0));
    let seen = calls.clone();
    let fixture = Fixture::new(move |r| {
        if let Some(reply) = token(&r, "/security/v1/oauth/token") {
            return Some(reply);
        }
        if r.path == "/api/shipments/v2409/pickup/oncall" {
            return Some(Reply::ok(
                r#"{"PickupRateResponse":{"Response":{"ResponseStatus":{"Code":"1"}},"RateResult":{"CurrencyCode":"USD","GrandTotalOfAllCharge":"8.50"}}}"#,
            ));
        }
        assert_eq!(r.path, "/api/pickupcreation/v2409/pickup");
        *seen.lock().unwrap() += 1;
        std::thread::sleep(std::time::Duration::from_millis(150));
        Some(Reply::ok(
            r#"{"PickupCreationResponse":{"Response":{"ResponseStatus":{"Code":"1"}},"PRN":"UPS-DRAIN-FIXTURE"}}"#,
        ))
    });
    let dir = tempfile::tempdir().unwrap();
    let store = crate::store::Store::open(&dir.path().join("packages.sqlite3"))
        .await
        .unwrap();
    let manager = service::Manager::unconfigured(&store).with_provider(Arc::new(
        ups::Ups::new(account("ABC123"))
            .unwrap()
            .with_origin(fixture.origin.clone()),
    ));
    manager
        .save_site(None, None, site_input(), true)
        .await
        .unwrap();
    let request = ScheduleRequest {
        carrier: Some(PickupCarrier::Ups),
        service: Some(PickupService::UpsGround),
        weight_lbs: Some("2.50".into()),
        accept_fee: true,
        ..Default::default()
    };
    let batch = manager
        .schedule("owner", "first-key", &request)
        .await
        .unwrap();
    let (stop, rx) = tokio::sync::watch::channel(false);
    let worker_manager = manager.clone();
    let worker = tokio::spawn(async move { worker_manager.tick(rx).await });
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while *calls.lock().unwrap() == 0 {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        manager.get(batch.items[0].id).await.unwrap().state,
        State::Dispatching
    );
    stop.send(true).unwrap();
    assert!(worker.await.unwrap().unwrap());
    let receipt = manager.get(batch.items[0].id).await.unwrap();
    assert_eq!(receipt.state, State::Scheduled);
    assert_eq!(receipt.confirmation.unwrap().number, "UPS-DRAIN-FIXTURE");
    let repeat = manager
        .schedule("another-owner", "new-generated-key", &request)
        .await
        .unwrap();
    assert_eq!(repeat.items[0].id, batch.items[0].id);
    assert_eq!(*calls.lock().unwrap(), 1);
    store.close().await.unwrap();
}
