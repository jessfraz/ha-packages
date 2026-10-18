use std::{
    io::{Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU16, Ordering},
    },
    thread,
    time::Duration,
};

use dropshot::HttpServer;
use reqwest::{Client, Method, StatusCode};
use serde::Serialize;
use shipments::{
    api,
    installation::{CheckStatus, InstallationReport},
    shipping_api::{ShippingApi, easypost::EasyPostClient},
    store::Store,
};

const OWNER: &str = "installation-owner-synthetic-credential-at-least-32-bytes";
const INTEGRATION: &str = "installation-adapter-synthetic-credential-at-least-32-bytes";
const FIRST_KEY: &str = "installation-synthetic-provider-key-one";
const SECOND_KEY: &str = "installation-synthetic-provider-key-two";

#[derive(Serialize)]
struct EmptyPage {
    trackers: [(); 0],
    has_more: bool,
}

// Store only the method and target. Authentication headers are never retained.
struct Provider {
    address: SocketAddr,
    requests: Arc<Mutex<Vec<(Method, String)>>>,
    status: Arc<AtomicU16>,
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl Provider {
    fn new() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let received = requests.clone();
        let status = Arc::new(AtomicU16::new(200));
        let reply_status = status.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        let thread = thread::spawn(move || {
            for stream in listener.incoming() {
                let mut stream = stream.unwrap();
                if stopping.load(Ordering::Acquire) {
                    break;
                }
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut bytes = Vec::new();
                while !bytes.ends_with(b"\r\n\r\n") {
                    let mut byte = [0];
                    stream.read_exact(&mut byte).unwrap();
                    bytes.push(byte[0]);
                    assert!(bytes.len() < 16 * 1024);
                }
                let headers = String::from_utf8(bytes).unwrap();
                let mut first = headers.lines().next().unwrap().split_whitespace();
                let method = first.next().unwrap().parse().unwrap();
                let target = first.next().unwrap().to_owned();
                received.lock().unwrap().push((method, target));
                let status = reply_status.load(Ordering::Acquire);
                let body = if status == 200 {
                    serde_json::to_string(&EmptyPage {
                        trackers: [],
                        has_more: false,
                    })
                    .unwrap()
                } else {
                    String::new()
                };
                write!(
                    stream,
                    "HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .unwrap();
            }
        });
        Self {
            address,
            requests,
            status,
            stop,
            thread: Some(thread),
        }
    }

    fn client(&self, key: &str) -> Arc<dyn ShippingApi> {
        Arc::new(EasyPostClient::for_loopback(key, &format!("http://{}/", self.address)).unwrap())
    }

    fn assert_probes(&self, count: usize) {
        let requests = self.requests.lock().unwrap();
        assert_eq!(requests.len(), count);
        for (method, target) in requests.iter() {
            assert_eq!(method, Method::GET);
            assert_eq!(target, "/trackers?page_size=1");
        }
    }
}

impl Drop for Provider {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        let _ = TcpStream::connect(self.address);
        self.thread.take().unwrap().join().unwrap();
    }
}

struct App {
    store: Store,
    server: HttpServer<Arc<api::Context>>,
    client: Client,
    base: String,
}

impl App {
    async fn open(path: &Path, provider: Option<Arc<dyn ShippingApi>>, integration: bool) -> Self {
        let store = Store::open(path).await.unwrap();
        let context = api::Context::new(store.clone(), OWNER, None, provider);
        let context = if integration {
            context.with_integration(INTEGRATION)
        } else {
            context
        };
        let server = api::start("127.0.0.1:0".parse().unwrap(), Arc::new(context)).unwrap();
        Self {
            base: format!("http://{}", server.local_addr()),
            server,
            store,
            client: Client::builder()
                .timeout(Duration::from_secs(5))
                .build()
                .unwrap(),
        }
    }

    async fn report(&self, method: Method) -> InstallationReport {
        let path = if method == Method::POST {
            "/v1/installation/check"
        } else {
            "/v1/installation"
        };
        let response = self
            .client
            .request(method, format!("{}{path}", self.base))
            .bearer_auth(OWNER)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        response.json().await.unwrap()
    }

    async fn close(self) {
        self.server.close().await.unwrap();
        self.store.close().await.unwrap();
    }
}

#[tokio::test]
async fn successful_installation_check_and_timestamp_survive_a_fresh_integrated_context() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("installation.sqlite3");
    let provider = Provider::new();
    let app = App::open(&path, Some(provider.client(FIRST_KEY)), false).await;
    let before = app.report(Method::GET).await;
    assert_eq!(before.easypost.status, CheckStatus::Attention);
    assert!(before.easypost.checked_at.is_none());
    provider.assert_probes(0);
    let checked = app.report(Method::POST).await;
    assert_eq!(checked.easypost.status, CheckStatus::Pass);
    assert!(checked.easypost.checked_at.is_some_and(|at| at > 0));
    provider.assert_probes(1);
    app.close().await;

    let restarted = App::open(&path, Some(provider.client(FIRST_KEY)), true).await;
    let saved = restarted.report(Method::GET).await;
    assert_eq!(saved.easypost.status, CheckStatus::Pass);
    assert_eq!(saved.easypost.checked_at, checked.easypost.checked_at);
    assert_eq!(saved.easypost.message, checked.easypost.message);
    provider.assert_probes(1);
    // Explicit checks may probe again after restart; simultaneous callers
    // still share one provider request and the existing one-minute debounce.
    let (first, second) = tokio::join!(
        restarted.report(Method::POST),
        restarted.report(Method::POST)
    );
    assert_eq!(first.easypost.status, CheckStatus::Pass);
    assert_eq!(second.easypost.status, CheckStatus::Pass);
    assert_eq!(first.easypost.checked_at, second.easypost.checked_at);
    provider.assert_probes(2);
    restarted.close().await;
}

#[tokio::test]
async fn changed_keys_or_endpoints_do_not_inherit_a_pass_and_disabled_provider_is_not_applicable() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("installation.sqlite3");
    let first_provider = Provider::new();
    let app = App::open(&path, Some(first_provider.client(FIRST_KEY)), true).await;
    assert_eq!(
        app.report(Method::POST).await.easypost.status,
        CheckStatus::Pass
    );
    app.close().await;
    let changed_key = App::open(&path, Some(first_provider.client(SECOND_KEY)), true).await;
    let unchecked = changed_key.report(Method::GET).await;
    assert_eq!(unchecked.easypost.status, CheckStatus::Attention);
    assert!(unchecked.easypost.checked_at.is_none());
    first_provider.assert_probes(1);
    assert_eq!(
        changed_key.report(Method::POST).await.easypost.status,
        CheckStatus::Pass
    );
    changed_key.close().await;
    first_provider.assert_probes(2);

    let other_provider = Provider::new();
    let changed_endpoint = App::open(&path, Some(other_provider.client(SECOND_KEY)), true).await;
    let unchecked = changed_endpoint.report(Method::GET).await;
    assert_eq!(unchecked.easypost.status, CheckStatus::Attention);
    assert!(unchecked.easypost.checked_at.is_none());
    other_provider.assert_probes(0);
    changed_endpoint.close().await;
    let disabled = App::open(&path, None, true).await;
    let report = disabled.report(Method::GET).await;
    assert_eq!(report.easypost.status, CheckStatus::NotApplicable);
    assert!(report.easypost.checked_at.is_none());
    other_provider.assert_probes(0);
    first_provider.assert_probes(2);
    disabled.close().await;
}

#[tokio::test]
async fn a_later_unauthorized_probe_replaces_success_and_persists_across_restart() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("installation.sqlite3");
    let provider = Provider::new();
    let app = App::open(&path, Some(provider.client(FIRST_KEY)), true).await;
    let passed = app.report(Method::POST).await;
    assert_eq!(passed.easypost.status, CheckStatus::Pass);
    app.close().await;
    provider.status.store(401, Ordering::Release);

    let restarted = App::open(&path, Some(provider.client(FIRST_KEY)), true).await;
    let failed = restarted.report(Method::POST).await;
    assert_eq!(failed.easypost.status, CheckStatus::Attention);
    assert!(failed.easypost.message.contains("401"));
    assert!(failed.easypost.checked_at.is_some());
    assert!(failed.easypost.checked_at >= passed.easypost.checked_at);
    provider.assert_probes(2);
    restarted.close().await;
    let restarted = App::open(&path, Some(provider.client(FIRST_KEY)), true).await;
    let saved = restarted.report(Method::GET).await;
    assert_eq!(saved.easypost.status, CheckStatus::Attention);
    assert_eq!(saved.easypost.checked_at, failed.easypost.checked_at);
    assert_eq!(saved.easypost.message, failed.easypost.message);
    provider.assert_probes(2);
    restarted.close().await;
}
