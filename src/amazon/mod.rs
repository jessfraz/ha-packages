//! Amazon account tracking. Credentials and website parsing stay here;
//! the ordinary provider worker owns saved tracking updates.

mod auth;
pub mod connection;
mod pages;

use std::{
    collections::{BTreeMap, BTreeSet},
    io::{Read, Write},
    net::IpAddr,
    path::PathBuf,
    sync::{
        Arc, RwLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};

use reqwest::{
    Client, Method, RequestBuilder, Url,
    cookie::{CookieStore, Jar},
    header::HeaderMap,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};
use tokio::{
    sync::{Mutex, MutexGuard},
    time::Instant,
};
use uuid::Uuid;

use crate::{
    amazon::connection::{ConnectionError, ConnectionReason, ConnectionStage},
    carrier::Carrier,
    shipping_api::{
        PreparedRegistration, ShippingApi,
        error::{Error, ErrorKind},
        types::{ApiFuture, ProviderId, Registration, Tracker},
    },
};

const MAX_BODY: usize = 4 * 1024 * 1024;
const MAX_TRACK_LOADS: usize = 10;
const FLOW_LIFETIME: Duration = Duration::from_secs(600);
const REQUEST_PAUSE: Duration = Duration::from_secs(1);

pub(crate) fn provider_error(kind: ErrorKind) -> Error {
    Error::new(ProviderId::AmazonAccount, kind)
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AmazonState {
    #[default]
    Disconnected,
    Connected,
    AuthRequired,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
pub struct AmazonStatus {
    pub state: AmazonState,
    pub last_success_at: Option<String>,
    pub last_error_code: Option<String>,
    pub imports_enabled: bool,
}

#[derive(Serialize, JsonSchema)]
pub struct SignIn {
    pub flow_id: String,
    pub url: String,
}

pub struct AmazonParcel {
    pub title: String,
    pub order_reference: String,
    pub source_key: String,
    pub tracker: Tracker,
}
pub struct Discovery {
    pub generation: u64,
    pub parcels: Vec<AmazonParcel>,
    pub deferred: usize,
}

// Deliberately no Debug: serde is only used for the owner-only credential file.
#[derive(Clone, Deserialize, Serialize)]
struct Credential {
    refresh_token: String,
    device_serial: String,
    customer_id: String,
    generation: u64,
    #[serde(default)]
    imports_enabled: bool,
    #[serde(default)]
    auth_required: bool,
    #[serde(default)]
    last_success_at: Option<String>,
}
struct Flow {
    id: String,
    owner: String,
    serial: String,
    verifier: String,
    expires: Instant,
}
struct State {
    credential: Option<Credential>,
    generation: u64,
    flow: Option<Flow>,
    last_error: Option<String>,
    cursor: usize,
    imported: BTreeSet<String>,
    observed: BTreeMap<String, Tracker>,
}
struct RequestSchedule {
    next: Instant,
    cooldown: Option<Instant>,
}

#[derive(Clone)]
pub struct AmazonClient {
    path: Arc<PathBuf>,
    state: Arc<Mutex<State>>,
    paused: Arc<AtomicBool>,
    epoch: Arc<AtomicU64>,
    request_generation: Option<u64>,
    session: Arc<Mutex<()>>,
    requests: Arc<Mutex<RequestSchedule>>,
    jar: Arc<RwLock<Arc<Jar>>>,
    http: Client,
    loopback: Option<Url>,
    pause: Duration,
}

/// Keep the account generation stable until the caller's short database commit.
/// No network request is made while this guard is held.
pub struct AmazonGuard<'a> {
    _state: MutexGuard<'a, State>,
}

impl crate::shipping_api::ObservationGuard for AmazonGuard<'_> {}

impl AmazonClient {
    pub fn new(path: PathBuf) -> Result<Self, Error> {
        let credential = load(&path)?;
        let generation = credential.as_ref().map_or(0, |value| value.generation);
        let paused = credential
            .as_ref()
            .is_none_or(|c| c.auth_required || !c.imports_enabled);
        let http = Client::builder().timeout(Duration::from_secs(20)).redirect(reqwest::redirect::Policy::none())
            .user_agent("Mozilla/5.0 (iPhone; CPU iPhone OS 17_5 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.5 Mobile/15E148 Safari/604.1")
            .build().map_err(|_| provider_error(ErrorKind::Configuration))?;
        Ok(Self {
            path: Arc::new(path),
            state: Arc::new(Mutex::new(State {
                credential,
                generation,
                flow: None,
                last_error: None,
                cursor: 0,
                imported: BTreeSet::new(),
                observed: BTreeMap::new(),
            })),
            paused: Arc::new(AtomicBool::new(paused)),
            epoch: Arc::new(AtomicU64::new(generation)),
            request_generation: None,
            session: Arc::new(Mutex::new(())),
            requests: Arc::new(Mutex::new(RequestSchedule {
                next: Instant::now(),
                cooldown: None,
            })),
            jar: Arc::new(RwLock::new(Arc::new(Jar::default()))),
            http,
            loopback: None,
            pause: REQUEST_PAUSE,
        })
    }

    /// Test transport accepts literal loopback only. Logical URLs and cookie
    /// domains still pass the production Amazon allowlist.
    pub fn for_test(path: PathBuf, endpoint: Url) -> Result<Self, Error> {
        if endpoint.scheme() != "http"
            || endpoint
                .host_str()
                .and_then(|h| h.parse::<IpAddr>().ok())
                .is_none_or(|ip| !ip.is_loopback())
            || !endpoint.username().is_empty()
            || endpoint.password().is_some()
            || endpoint.query().is_some()
            || endpoint.fragment().is_some()
        {
            return Err(provider_error(ErrorKind::Configuration));
        }
        let mut client = Self::new(path)?;
        client.loopback = Some(endpoint);
        client.pause = Duration::ZERO;
        Ok(client)
    }

    pub async fn status(&self) -> AmazonStatus {
        let state = self.state.lock().await;
        match &state.credential {
            None => AmazonStatus {
                last_error_code: state.last_error.clone(),
                ..Default::default()
            },
            Some(c) => AmazonStatus {
                state: if c.auth_required {
                    AmazonState::AuthRequired
                } else {
                    AmazonState::Connected
                },
                last_success_at: c.last_success_at.clone(),
                last_error_code: state.last_error.clone(),
                imports_enabled: c.imports_enabled,
            },
        }
    }
    pub async fn generation(&self) -> u64 {
        self.state.lock().await.generation
    }
    pub(crate) async fn tracking_scope(&self) -> Option<String> {
        self.state
            .lock()
            .await
            .credential
            .as_ref()
            .map(|credential| format!("{}:", account_key(&credential.customer_id)))
    }
    pub async fn commit_guard(&self, generation: u64) -> Result<AmazonGuard<'_>, Error> {
        let state = self.state.lock().await;
        if state.generation != generation
            || state
                .credential
                .as_ref()
                .is_none_or(|c| c.auth_required || !c.imports_enabled)
        {
            return Err(provider_error(ErrorKind::AuthenticationRequired));
        }
        Ok(AmazonGuard { _state: state })
    }
    pub async fn remember_imported(&self, keys: &[String]) {
        let mut state = self.state.lock().await;
        for key in keys {
            state.imported.insert(key.clone());
        }
        while state.imported.len() > 10_000 {
            state.imported.pop_first();
        }
    }

    pub async fn begin_sign_in(&self, owner: &str) -> Result<SignIn, Error> {
        if owner.is_empty() || owner.len() > 256 || owner.chars().any(char::is_control) {
            return Err(provider_error(ErrorKind::InvalidResponse));
        }
        let flow = Flow {
            id: Uuid::now_v7().to_string(),
            owner: owner.into(),
            serial: Uuid::now_v7().simple().to_string().to_ascii_uppercase(),
            verifier: auth::verifier()?,
            expires: Instant::now() + FLOW_LIFETIME,
        };
        let result = SignIn {
            flow_id: flow.id.clone(),
            url: auth::sign_in(&flow.serial, &flow.verifier)?,
        };
        self.state.lock().await.flow = Some(flow);
        Ok(result)
    }

    pub async fn complete_sign_in(
        &self,
        owner: &str,
        flow_id: &str,
        landing_url: &str,
    ) -> Result<AmazonStatus, ConnectionError> {
        let generation = self.generation().await;
        let result = self
            .complete_sign_in_inner(owner, flow_id, landing_url)
            .await;
        if let Err(error) = &result {
            let mut state = self.state.lock().await;
            if state.generation == generation && state.credential.is_none() {
                state.last_error = Some(error.code());
            }
        }
        result
    }

    async fn complete_sign_in_inner(
        &self,
        owner: &str,
        flow_id: &str,
        landing_url: &str,
    ) -> Result<AmazonStatus, ConnectionError> {
        let code = auth::code(landing_url).map_err(|_| {
            ConnectionError::failure(ConnectionStage::Callback, ConnectionReason::InvalidInput)
        })?;
        let (flow, generation) = {
            let mut state = self.state.lock().await;
            // A wrong caller cannot consume someone else's flow.
            let flow = state
                .flow
                .as_ref()
                .filter(|f| f.id == flow_id && f.owner == owner && f.expires > Instant::now())
                .ok_or_else(|| {
                    ConnectionError::failure(ConnectionStage::Flow, ConnectionReason::ExpiredFlow)
                })?;
            let id = flow.id.clone();
            let flow = state.flow.take().filter(|f| f.id == id).ok_or_else(|| {
                ConnectionError::failure(ConnectionStage::Flow, ConnectionReason::ExpiredFlow)
            })?;
            (flow, state.generation)
        };
        let _session = self.session.lock().await;
        let registration = auth::register(self, &flow.serial, &flow.verifier, &code).await?;
        {
            let state = self.state.lock().await;
            if state.generation != generation {
                return Err(ConnectionError::failure(
                    ConnectionStage::Account,
                    ConnectionReason::SessionChanged,
                ));
            }
            if state
                .credential
                .as_ref()
                .is_some_and(|c| c.customer_id != registration.customer)
            {
                return Err(ConnectionError::failure(
                    ConnectionStage::Account,
                    ConnectionReason::AccountMismatch,
                ));
            }
        }
        // Registration identity and a successful access-token grant prove sign-in.
        // Website cookies and order layouts belong to Preview, not credential durability.
        let replacement = auth::refresh_access_token(self, &registration.token).await?;
        let token = replacement.unwrap_or(registration.token);
        let mut state = self.state.lock().await;
        if state.generation != generation {
            return Err(ConnectionError::failure(
                ConnectionStage::Account,
                ConnectionReason::SessionChanged,
            ));
        }
        let imports_enabled = state.credential.as_ref().is_some_and(|c| c.imports_enabled);
        let credential = Credential {
            refresh_token: token,
            device_serial: flow.serial,
            customer_id: registration.customer,
            generation: generation.saturating_add(1),
            imports_enabled,
            auth_required: false,
            last_success_at: Some(
                now().map_err(|e| ConnectionError::new(ConnectionStage::Credentials, e))?,
            ),
        };
        save(&self.path, &credential).map_err(|_| {
            ConnectionError::failure(ConnectionStage::Credentials, ConnectionReason::Persistence)
        })?;
        state.generation = credential.generation;
        self.epoch.store(credential.generation, Ordering::Release);
        state.credential = Some(credential);
        state.last_error = None;
        state.cursor = 0;
        state.observed.clear();
        self.paused.store(!imports_enabled, Ordering::Release);
        drop(state);
        Ok(self.status().await)
    }

    pub async fn disconnect(&self) -> Result<AmazonStatus, Error> {
        let _session = self.session.lock().await;
        let mut state = self.state.lock().await;
        match std::fs::remove_file(&*self.path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err(provider_error(ErrorKind::Configuration)),
        }
        state.generation = state.generation.saturating_add(1);
        self.epoch.store(state.generation, Ordering::Release);
        state.credential = None;
        state.flow = None;
        state.last_error = None;
        state.imported.clear();
        state.observed.clear();
        self.clear_cookies()?;
        self.paused.store(true, Ordering::Release);
        Ok(AmazonStatus::default())
    }

    pub async fn enable_imports(&self) -> Result<AmazonStatus, Error> {
        let mut state = self.state.lock().await;
        let mut credential = state
            .credential
            .as_ref()
            .filter(|c| !c.auth_required)
            .cloned()
            .ok_or_else(|| provider_error(ErrorKind::AuthenticationRequired))?;
        credential.imports_enabled = true;
        save(&self.path, &credential)?;
        state.credential = Some(credential);
        state.cursor = 0;
        self.paused.store(false, Ordering::Release);
        drop(state);
        Ok(self.status().await)
    }

    pub async fn discover(&self) -> Result<Discovery, Error> {
        self.preview().await.map_err(ConnectionError::into_provider)
    }

    pub async fn preview(&self) -> Result<Discovery, ConnectionError> {
        let generation = self.generation().await;
        let result = self.discover_inner().await;
        if let Err(error) = &result {
            self.record_error(generation, error.provider_error())
                .await
                .map_err(|e| ConnectionError::new(ConnectionStage::Credentials, e))?;
        }
        result
    }
    async fn discover_inner(&self) -> Result<Discovery, ConnectionError> {
        let (generation, customer) = {
            let state = self.state.lock().await;
            let c = state
                .credential
                .as_ref()
                .filter(|c| !c.auth_required)
                .ok_or_else(|| {
                    ConnectionError::new(
                        ConnectionStage::AccessToken,
                        provider_error(ErrorKind::AuthenticationRequired),
                    )
                })?;
            (state.generation, c.customer_id.clone())
        };
        // Validate the saved refresh token once for the account's discovery
        // cycle, including when no parcels exist. Per-parcel reads share cookies.
        {
            let _session = self.session.lock().await;
            self.authenticate_saved(generation).await?;
        }
        let listing = self.order_listing().await?;
        let mut parcels = Vec::new();
        let mut deferred = listing.deferred;
        let (start, imported) = {
            let state = self.state.lock().await;
            (state.cursor, state.imported.clone())
        };
        let count = listing.parcels.len();
        let mut reads = 0;
        for offset in 0..count {
            let index = (start + offset) % count;
            let parcel = &listing.parcels[index];
            let source_key = source_key(&customer, parcel);
            if imported.contains(&source_key) {
                continue;
            }
            if reads >= MAX_TRACK_LOADS {
                deferred += 1;
                continue;
            }
            reads += 1;
            self.state.lock().await.cursor = (index + 1) % count;
            let result = async {
                let mut path = parcel.path.clone();
                let mut page = self.fetch_page_detailed(&path).await?;
                if !pages::is_tracking(&path) {
                    path = pages::tracking_link(&page, parcel)
                        .map_err(|e| ConnectionError::new(ConnectionStage::Orders, e))?;
                    page = self.fetch_page_detailed(&path).await?;
                }
                pages::tracking(&page, &path, None)
                    .map_err(|e| ConnectionError::new(ConnectionStage::Orders, e))
            }
            .await;
            match result {
                Ok(mut tracker) => {
                    tracker.id = format!("{}:{}", account_key(&customer), tracker.id);
                    let title = parcel.titles.join("; ");
                    if title.len() > 500 {
                        deferred += 1;
                        continue;
                    }
                    parcels.push(AmazonParcel {
                        title,
                        order_reference: parcel.order.clone(),
                        source_key,
                        tracker,
                    });
                }
                Err(error)
                    if error.stage == ConnectionStage::Orders
                        && matches!(error.provider_error().kind, ErrorKind::InvalidResponse) =>
                {
                    deferred += 1;
                }
                Err(error) => return Err(error),
            }
        }
        let mut state = self.state.lock().await;
        if state.generation != generation {
            return Err(ConnectionError::failure(
                ConnectionStage::Account,
                ConnectionReason::SessionChanged,
            ));
        }
        for parcel in &parcels {
            state
                .observed
                .insert(parcel.tracker.tracking_code.clone(), parcel.tracker.clone());
        }
        while state.observed.len() > 500 {
            state.observed.pop_first();
        }
        if let Some(c) = &mut state.credential {
            c.last_success_at =
                Some(now().map_err(|e| ConnectionError::new(ConnectionStage::Credentials, e))?);
            save(&self.path, c)
                .map_err(|e| ConnectionError::new(ConnectionStage::Credentials, e))?;
        }
        state.last_error = None;
        Ok(Discovery {
            generation,
            parcels,
            deferred,
        })
    }

    pub(crate) fn request(&self, method: Method, logical: &str) -> Result<RequestBuilder, Error> {
        let url = Url::parse(logical).map_err(|_| provider_error(ErrorKind::InvalidResponse))?;
        if url.scheme() != "https"
            || !["www.amazon.com", "api.amazon.com"].contains(&url.host_str().unwrap_or_default())
            || !url.username().is_empty()
            || url.password().is_some()
            || url.port().is_some_and(|p| p != 443)
            || url.fragment().is_some()
        {
            return Err(provider_error(ErrorKind::InvalidResponse));
        }
        let target = match &self.loopback {
            Some(base) => base
                .join(&format!(
                    "{}{}",
                    url.path(),
                    url.query().map(|q| format!("?{q}")).unwrap_or_default()
                ))
                .map_err(|_| provider_error(ErrorKind::Configuration))?,
            None => url.clone(),
        };
        let mut request = self.http.request(method, target);
        if url.host_str() == Some("www.amazon.com") {
            let jar = self
                .jar
                .read()
                .map_err(|_| provider_error(ErrorKind::Configuration))?;
            if let Some(cookies) = jar.cookies(&url) {
                request = request.header(reqwest::header::COOKIE, cookies);
            }
        }
        Ok(request)
    }
    pub(crate) fn replace_cookies(&self, jar: Jar) -> Result<(), Error> {
        *self
            .jar
            .write()
            .map_err(|_| provider_error(ErrorKind::Configuration))? = Arc::new(jar);
        Ok(())
    }
    fn clear_cookies(&self) -> Result<(), Error> {
        self.replace_cookies(Jar::default())
    }

    async fn response(&self, request: RequestBuilder) -> Result<(u16, HeaderMap, Vec<u8>), Error> {
        let mut slot = self.requests.lock().await;
        if let Some(until) = slot.cooldown.filter(|until| *until > Instant::now()) {
            return Err(provider_error(ErrorKind::Throttled {
                retry_after: i64::try_from((until - Instant::now()).as_secs())
                    .unwrap_or(21600)
                    .max(1),
            }));
        }
        tokio::time::sleep_until(slot.next).await;
        slot.next = Instant::now() + self.pause;
        let mut response = request.send().await.map_err(|e| {
            provider_error(if e.is_timeout() {
                ErrorKind::Timeout
            } else {
                ErrorKind::Connection
            })
        })?;
        let status = response.status().as_u16();
        let headers = response.headers().clone();
        // Website responses may rotate their own session cookies. Never accept
        // cookies from an arbitrary redirect target or from the test transport.
        if response.url().scheme() == "https" && response.url().host_str() == Some("www.amazon.com")
        {
            let jar = self
                .jar
                .read()
                .map_err(|_| provider_error(ErrorKind::Configuration))?;
            for value in headers.get_all(reqwest::header::SET_COOKIE) {
                if let Ok(value) = value.to_str() {
                    jar.add_cookie_str(value, response.url());
                }
            }
        }
        if response
            .content_length()
            .is_some_and(|length| length > MAX_BODY as u64)
        {
            return Err(provider_error(ErrorKind::ResponseTooLarge));
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| provider_error(ErrorKind::Connection))?
        {
            if chunk.len() > MAX_BODY.saturating_sub(bytes.len()) {
                return Err(provider_error(ErrorKind::ResponseTooLarge));
            }
            bytes.extend_from_slice(&chunk);
        }
        if matches!(status, 429 | 503) {
            slot.cooldown = Some(
                Instant::now() + Duration::from_secs(retry_after(&headers).unwrap_or(60) as u64),
            );
        }
        Ok((status, headers, bytes))
    }
    pub(crate) async fn json<T: DeserializeOwned>(
        &self,
        stage: ConnectionStage,
        request: RequestBuilder,
    ) -> Result<T, ConnectionError> {
        let (status, headers, body) = self
            .response(request)
            .await
            .map_err(|e| ConnectionError::new(stage, e))?;
        if status != 200 {
            let kind = if matches!(status, 400 | 401 | 403) && confirmed_rejection(&body) {
                ErrorKind::AuthenticationRequired
            } else if matches!(status, 401 | 403) {
                ErrorKind::InvalidResponse
            } else {
                ErrorKind::Http {
                    status,
                    retry_after: retry_after(&headers),
                }
            };
            return Err(ConnectionError::with_status(
                stage,
                provider_error(kind),
                status,
            ));
        }
        serde_json::from_slice(&body).map_err(|_| {
            ConnectionError::with_status(stage, provider_error(ErrorKind::InvalidResponse), status)
        })
    }

    async fn page_once(&self, path: &str) -> Result<Page, ConnectionError> {
        let stage = ConnectionStage::Orders;
        let mut path = pages::allowed_path(path).map_err(|e| ConnectionError::new(stage, e))?;
        for _ in 0..3 {
            let request = self
                .request(Method::GET, &format!("{}{path}", pages::ORIGIN))
                .map_err(|e| ConnectionError::new(stage, e))?;
            let (status, headers, body) = self
                .response(request)
                .await
                .map_err(|e| ConnectionError::new(stage, e))?;
            let invalid = || {
                ConnectionError::with_status(
                    stage,
                    provider_error(ErrorKind::InvalidResponse),
                    status,
                )
            };
            if (300..400).contains(&status) {
                let location = headers
                    .get(reqwest::header::LOCATION)
                    .and_then(|v| v.to_str().ok())
                    .ok_or_else(invalid)?;
                let base =
                    Url::parse(&format!("{}{path}", pages::ORIGIN)).map_err(|_| invalid())?;
                let url = base.join(location).map_err(|_| invalid())?;
                if url.scheme() == "https"
                    && url.host_str() == Some("www.amazon.com")
                    && url.path() == "/ap/signin"
                    && url.username().is_empty()
                    && url.password().is_none()
                    && url.port().is_none()
                {
                    return Ok(Page::SignedOut(status));
                }
                let destination = pages::allowed_path(url.as_str()).map_err(|_| invalid())?;
                if !pages::same_parcel_identity(&path, &destination) {
                    return Err(invalid());
                }
                path = destination;
                continue;
            }
            if status != 200 {
                let kind = if matches!(status, 401 | 403) {
                    ErrorKind::InvalidResponse
                } else {
                    ErrorKind::Http {
                        status,
                        retry_after: retry_after(&headers),
                    }
                };
                return Err(ConnectionError::with_status(
                    stage,
                    provider_error(kind),
                    status,
                ));
            }
            let page = String::from_utf8(body).map_err(|_| {
                let shape = match headers
                    .get(reqwest::header::CONTENT_ENCODING)
                    .and_then(|v| v.to_str().ok())
                {
                    Some("gzip") => "encoded_gzip",
                    Some("br") => "encoded_brotli",
                    Some("deflate") => "encoded_deflate",
                    Some(_) => "encoded_other",
                    None => "invalid_utf8",
                };
                invalid().with_shape(shape)
            })?;
            return Ok(if pages::signed_out(&page) {
                Page::SignedOut(status)
            } else {
                Page::Body(page)
            });
        }
        Err(ConnectionError::new(
            stage,
            provider_error(ErrorKind::InvalidResponse),
        ))
    }
    // Caller holds the shared session lock. A replacement is durable before
    // website access, so a cookie outage cannot strand an otherwise valid grant.
    async fn authenticate_saved(&self, generation: u64) -> Result<(), ConnectionError> {
        let token = {
            let state = self.state.lock().await;
            if state.generation != generation {
                return Err(ConnectionError::failure(
                    ConnectionStage::Account,
                    ConnectionReason::SessionChanged,
                ));
            }
            state
                .credential
                .as_ref()
                .filter(|c| !c.auth_required)
                .ok_or_else(|| {
                    ConnectionError::new(
                        ConnectionStage::AccessToken,
                        provider_error(ErrorKind::AuthenticationRequired),
                    )
                })?
                .refresh_token
                .clone()
        };
        let replacement = auth::refresh_access_token(self, &token).await?;
        if let Some(replacement) = &replacement {
            let mut state = self.state.lock().await;
            let credential = state
                .credential
                .as_ref()
                .filter(|c| state.generation == generation && c.refresh_token == token)
                .ok_or_else(|| {
                    ConnectionError::failure(
                        ConnectionStage::Account,
                        ConnectionReason::SessionChanged,
                    )
                })?;
            let mut updated = credential.clone();
            updated.refresh_token = replacement.clone();
            save(&self.path, &updated).map_err(|_| {
                ConnectionError::failure(
                    ConnectionStage::Credentials,
                    ConnectionReason::Persistence,
                )
            })?;
            state.credential = Some(updated);
        }
        auth::exchange_cookies(self, replacement.as_deref().unwrap_or(&token)).await
    }

    async fn order_listing(&self) -> Result<pages::Listing, ConnectionError> {
        let mut last = ConnectionError::failure(
            ConnectionStage::Orders,
            ConnectionReason::UnsupportedOrdersLayout,
        );
        for path in ["/your-orders/orders", "/gp/css/order-history"] {
            match self.fetch_page_detailed(path).await {
                Ok(page) => match pages::listing(&page) {
                    Ok(listing) => return Ok(listing),
                    Err(error) if matches!(error.kind, ErrorKind::InvalidResponse) => {
                        let shape = pages::orders_shape(&page);
                        let reason = if shape == "challenge" {
                            ConnectionReason::ChallengePage
                        } else {
                            ConnectionReason::UnsupportedOrdersLayout
                        };
                        last = ConnectionError::failure(ConnectionStage::Orders, reason)
                            .with_upstream_status(200)
                            .with_shape(shape);
                        if reason == ConnectionReason::ChallengePage {
                            return Err(last);
                        }
                    }
                    Err(error) => {
                        return Err(ConnectionError::with_status(
                            ConnectionStage::Orders,
                            error,
                            200,
                        ));
                    }
                },
                Err(error)
                    if error.stage == ConnectionStage::Orders
                        && error.upstream_status == Some(404) =>
                {
                    last = error
                }
                Err(error) => return Err(error),
            }
        }
        Err(last)
    }

    async fn fetch_page(&self, path: &str) -> Result<String, Error> {
        self.fetch_page_detailed(path)
            .await
            .map_err(ConnectionError::into_provider)
    }
    async fn fetch_page_detailed(&self, path: &str) -> Result<String, ConnectionError> {
        let _session = self.session.lock().await;
        let generation = self.generation().await;
        let origin = Url::parse(pages::ORIGIN).map_err(|_| {
            ConnectionError::new(
                ConnectionStage::Cookies,
                provider_error(ErrorKind::Configuration),
            )
        })?;
        let empty = self
            .jar
            .read()
            .map_err(|_| {
                ConnectionError::new(
                    ConnectionStage::Cookies,
                    provider_error(ErrorKind::Configuration),
                )
            })?
            .cookies(&origin)
            .is_none();
        if empty {
            self.authenticate_saved(generation).await?;
        }
        match self.page_once(path).await? {
            Page::Body(page) => Ok(page),
            Page::SignedOut(_) => {
                self.clear_cookies()
                    .map_err(|e| ConnectionError::new(ConnectionStage::Cookies, e))?;
                self.authenticate_saved(generation).await?;
                match self.page_once(path).await? {
                    Page::Body(page) => Ok(page),
                    Page::SignedOut(status) => Err(ConnectionError::failure(
                        ConnectionStage::Orders,
                        ConnectionReason::ChallengePage,
                    )
                    .with_upstream_status(status)
                    .with_shape("sign_in")),
                }
            }
        }
    }
    async fn record_error(&self, generation: u64, error: &Error) -> Result<(), Error> {
        let mut state = self.state.lock().await;
        if state.generation == generation {
            state.last_error = Some(error.error_code().into());
            if matches!(error.kind, ErrorKind::AuthenticationRequired) {
                self.paused.store(true, Ordering::Release);
                if let Some(c) = &mut state.credential {
                    c.auth_required = true;
                    save(&self.path, c)?;
                }
            }
        }
        Ok(())
    }
    async fn retrieve_page(&self, path: &str, number: &str) -> Result<Tracker, Error> {
        let generation = self.request_generation.unwrap_or(self.generation().await);
        let result = self.retrieve_inner(path, number).await;
        if let Err(error) = &result {
            self.record_error(generation, error).await?;
        }
        result
    }
    async fn retrieve_inner(&self, path: &str, number: &str) -> Result<Tracker, Error> {
        let (generation, customer) = {
            let state = self.state.lock().await;
            let credential = state
                .credential
                .as_ref()
                .filter(|c| !c.auth_required)
                .ok_or_else(|| provider_error(ErrorKind::AuthenticationRequired))?;
            (state.generation, credential.customer_id.clone())
        };
        if self
            .request_generation
            .is_some_and(|captured| captured != generation)
        {
            return Err(provider_error(ErrorKind::AuthenticationRequired));
        }
        let (account, path) = path
            .split_once(':')
            .ok_or_else(|| provider_error(ErrorKind::InvalidResponse))?;
        if account != account_key(&customer) {
            return Err(provider_error(ErrorKind::InvalidResponse));
        }
        let page = self.fetch_page(path).await?;
        let tracker = pages::tracking(&page, path, Some(number))?;
        let mut state = self.state.lock().await;
        if state.generation != generation {
            return Err(provider_error(ErrorKind::AuthenticationRequired));
        }
        if let Some(c) = &mut state.credential {
            c.last_success_at = Some(now()?);
            save(&self.path, c)?;
        }
        state.last_error = None;
        let mut tracker = tracker;
        tracker.id = format!("{}:{}", account_key(&customer), tracker.id);
        Ok(tracker)
    }
}

enum Page {
    Body(String),
    SignedOut(u16),
}

fn retry_after(headers: &HeaderMap) -> Option<i64> {
    headers
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<i64>().ok())
        .map(|v| v.clamp(1, 21600))
}
fn now() -> Result<String, Error> {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .map_err(|_| provider_error(ErrorKind::Configuration))
}
fn account_key(customer: &str) -> String {
    let mut hash = Sha256::new();
    hash.update(b"amazon.com\0");
    hash.update(customer.as_bytes());
    hash.finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}
fn source_key(customer: &str, parcel: &pages::ParcelPage) -> String {
    let mut hash = Sha256::new();
    for part in [
        "amazon.com",
        customer,
        &parcel.order,
        &parcel.shipment,
        &parcel.package,
    ] {
        hash.update(part.as_bytes());
        hash.update([0]);
    }
    hash.finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}
#[derive(Deserialize)]
#[serde(untagged)]
enum AuthCode {
    Text(String),
    Detail { code: String },
}
#[derive(Default, Deserialize)]
#[serde(default)]
struct AuthFailure {
    error: Option<AuthCode>,
    response: Option<AuthFailureResponse>,
}
#[derive(Deserialize)]
struct AuthFailureResponse {
    error: Option<AuthCode>,
}
fn confirmed_rejection(body: &[u8]) -> bool {
    let Ok(parsed) = serde_json::from_slice::<AuthFailure>(body) else {
        return false;
    };
    let code = parsed
        .error
        .or_else(|| parsed.response.and_then(|r| r.error));
    code.is_some_and(|c| {
        let code = match c {
            AuthCode::Text(c) | AuthCode::Detail { code: c } => c,
        };
        ["invalid_grant", "invalid_token", "invalid_refresh_token"].contains(&code.as_str())
    })
}
fn load(path: &std::path::Path) -> Result<Option<Credential>, Error> {
    use std::os::unix::fs::PermissionsExt;
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(provider_error(ErrorKind::Configuration)),
    };
    if !metadata.is_file() || metadata.permissions().mode() & 0o077 != 0 || metadata.len() > 16384 {
        return Err(provider_error(ErrorKind::Configuration));
    }
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .and_then(|f| f.take(16385).read_to_end(&mut bytes))
        .map_err(|_| provider_error(ErrorKind::Configuration))?;
    let c: Credential =
        serde_json::from_slice(&bytes).map_err(|_| provider_error(ErrorKind::Configuration))?;
    if c.refresh_token.is_empty()
        || c.refresh_token.len() > 8192
        || c.customer_id.is_empty()
        || c.customer_id.len() > 256
        || c.device_serial.len() > 128
        || c.generation == 0
    {
        return Err(provider_error(ErrorKind::Configuration));
    }
    Ok(Some(c))
}
fn save(path: &std::path::Path, credential: &Credential) -> Result<(), Error> {
    use std::os::unix::fs::OpenOptionsExt;
    let temporary = path.with_extension(format!("{}.tmp", Uuid::now_v7()));
    let result = (|| {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)
            .map_err(|_| provider_error(ErrorKind::Configuration))?;
        f.write_all(
            &serde_json::to_vec(credential)
                .map_err(|_| provider_error(ErrorKind::Configuration))?,
        )
        .and_then(|_| f.sync_all())
        .map_err(|_| provider_error(ErrorKind::Configuration))?;
        std::fs::rename(&temporary, path).map_err(|_| provider_error(ErrorKind::Configuration))?;
        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| std::path::Path::new("."));
        std::fs::File::open(parent)
            .and_then(|f| f.sync_all())
            .map_err(|_| provider_error(ErrorKind::Configuration))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

impl ShippingApi for AmazonClient {
    fn id(&self) -> ProviderId {
        ProviderId::AmazonAccount
    }
    fn blocked(&self) -> bool {
        self.paused.load(Ordering::Acquire)
    }
    fn supports_carrier(&self, carrier: Carrier) -> bool {
        pages::supports_carrier(carrier)
    }
    fn for_request(&self, _request_id: &str, _shipment_id: Option<&str>) -> Arc<dyn ShippingApi> {
        let mut scoped = self.clone();
        scoped.request_generation = Some(self.epoch.load(Ordering::Acquire));
        Arc::new(scoped)
    }
    fn observation_guard(
        &self,
    ) -> ApiFuture<'_, Box<dyn crate::shipping_api::ObservationGuard + '_>> {
        Box::pin(async move {
            let captured = self
                .request_generation
                .ok_or_else(|| provider_error(ErrorKind::Configuration))?;
            let state = self.state.lock().await;
            if state.generation != captured
                || state.credential.as_ref().is_none_or(|c| !c.imports_enabled)
            {
                return Err(provider_error(ErrorKind::AuthenticationRequired));
            }
            Ok(Box::new(AmazonGuard { _state: state })
                as Box<dyn crate::shipping_api::ObservationGuard>)
        })
    }
    fn check_connection(&self) -> ApiFuture<'_, ()> {
        Box::pin(async { self.discover().await.map(|_| ()) })
    }
    fn reconcile_registration<'a>(
        &'a self,
        registration: &'a Registration,
    ) -> ApiFuture<'a, Option<Tracker>> {
        Box::pin(async move {
            let tracker = self
                .state
                .lock()
                .await
                .observed
                .get(&registration.tracking_number)
                .cloned()
                .ok_or_else(|| provider_error(ErrorKind::RegistrationAmbiguous))?;
            if registration.carrier.is_some_and(|c| c != tracker.carrier) {
                return Err(provider_error(ErrorKind::InvalidResponse));
            }
            self.retrieve_page(&tracker.id, &registration.tracking_number)
                .await
                .map(Some)
        })
    }
    fn prepare_registration(
        &self,
        registration: Registration,
    ) -> ApiFuture<'_, Box<dyn PreparedRegistration>> {
        Box::pin(async move {
            let tracker = self
                .state
                .lock()
                .await
                .observed
                .get(&registration.tracking_number)
                .cloned()
                .ok_or_else(|| provider_error(ErrorKind::RegistrationAmbiguous))?;
            if registration.carrier.is_some_and(|c| c != tracker.carrier) {
                return Err(provider_error(ErrorKind::InvalidResponse));
            }
            Ok(Box::new(AccountRegistration {
                client: self.clone(),
                path: tracker.id,
                number: registration.tracking_number,
            }) as Box<dyn PreparedRegistration>)
        })
    }
    fn retrieve<'a>(
        &'a self,
        tracker_id: &'a str,
        tracking_number: &'a str,
    ) -> ApiFuture<'a, Tracker> {
        Box::pin(self.retrieve_page(tracker_id, tracking_number))
    }
}
struct AccountRegistration {
    client: AmazonClient,
    path: String,
    number: String,
}
impl PreparedRegistration for AccountRegistration {
    fn register(self: Box<Self>) -> ApiFuture<'static, Tracker> {
        Box::pin(async move { self.client.retrieve_page(&self.path, &self.number).await })
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        amazon::{AmazonClient, AmazonState, Credential, account_key, save},
        shipping_api::{ShippingApi, error::ErrorKind},
    };
    use std::{
        io::{Read, Write},
        net::TcpListener,
        sync::{
            Arc, Mutex,
            atomic::{AtomicBool, Ordering},
        },
        thread,
    };
    const PATH: &str =
        "/gp/your-account/ship-track?orderId=111-1234567-1234567&shipmentId=SHIP1&packageId=1";
    const NUMBER: &str = "9400111122223333444455";
    fn listing() -> String {
        format!(
            r#"<a class="item-card__link" href="{PATH}" aria-label="Widget, Order placed 1 October 2026, Shipped"></a>"#
        )
    }
    fn tracking() -> String {
        format!(
            r#"<h3>Delivery By USPS</h3><script id="page-state">{{"trackingId":"{NUMBER}","shortStatus":"IN_TRANSIT"}}</script>"#
        )
    }
    const ACCESS: &str = r#"{"access_token":"SYNTHETIC_ACCESS"}"#;
    const COOKIES: &str = r#"{"response":{"tokens":{"cookies":{".amazon.com":[{"Name":"session-id","Value":"SYNTHETIC_COOKIE","Path":"/"}]}}}}"#;
    const REGISTER: &str = r#"{"response":{"success":{"tokens":{"bearer":{"refresh_token":"SYNTHETIC_REFRESH"}},"extensions":{"customer_info":{"user_id":"SYNTHETIC_CUSTOMER"}}}}}"#;
    struct Fixture {
        url: reqwest::Url,
        calls: Arc<Mutex<Vec<String>>>,
        stop: Arc<AtomicBool>,
        thread: Option<thread::JoinHandle<()>>,
    }
    impl Fixture {
        fn new(handler: impl Fn(&str) -> (u16, String) + Send + 'static) -> Self {
            Self::with_location(move |path| {
                let (status, body) = handler(path);
                (status, None, body)
            })
        }
        fn with_location(
            handler: impl Fn(&str) -> (u16, Option<String>, String) + Send + 'static,
        ) -> Self {
            Self::with_response(move |path| {
                let (status, location, body) = handler(path);
                let headers = location
                    .map(|value| format!("Location: {value}\r\n"))
                    .unwrap_or_default();
                (status, headers, body.into_bytes())
            })
        }
        fn with_response(
            handler: impl Fn(&str) -> (u16, String, Vec<u8>) + Send + 'static,
        ) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let url = reqwest::Url::parse(&format!("http://{}/", listener.local_addr().unwrap()))
                .unwrap();
            let calls = Arc::new(Mutex::new(Vec::new()));
            let stop = Arc::new(AtomicBool::new(false));
            let thread_calls = calls.clone();
            let thread_stop = stop.clone();
            let thread = thread::spawn(move || {
                while !thread_stop.load(Ordering::Acquire) {
                    let (mut stream, _) = match listener.accept() {
                        Ok(v) => v,
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(std::time::Duration::from_millis(2));
                            continue;
                        }
                        Err(_) => break,
                    };
                    // Darwin may inherit the listener's nonblocking mode.
                    // Each accepted fixture connection must wait for its request.
                    stream.set_nonblocking(false).unwrap();
                    stream
                        .set_read_timeout(Some(std::time::Duration::from_secs(2)))
                        .unwrap();
                    let mut bytes = Vec::new();
                    let mut buffer = [0u8; 4096];
                    let header_end = loop {
                        let n = stream.read(&mut buffer).unwrap_or(0);
                        if n == 0 {
                            break None;
                        }
                        bytes.extend_from_slice(&buffer[..n]);
                        if let Some(i) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                            break Some(i + 4);
                        }
                        if bytes.len() > 32768 {
                            break None;
                        }
                    };
                    let Some(header_end) = header_end else {
                        continue;
                    };
                    let headers = String::from_utf8_lossy(&bytes[..header_end]);
                    let path = headers
                        .lines()
                        .next()
                        .unwrap()
                        .split_whitespace()
                        .nth(1)
                        .unwrap()
                        .to_owned();
                    let length = headers
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length: ")
                                .and_then(|s| s.parse::<usize>().ok())
                        })
                        .unwrap_or(0);
                    while bytes.len() < header_end + length {
                        let n = stream.read(&mut buffer).unwrap_or(0);
                        if n == 0 {
                            break;
                        }
                        bytes.extend_from_slice(&buffer[..n]);
                    }
                    thread_calls.lock().unwrap().push(path.clone());
                    let (status, headers, body) = handler(&path);
                    let reply = format!(
                        "HTTP/1.1 {status} fixture\r\n{headers}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = stream.write_all(reply.as_bytes());
                    let _ = stream.write_all(&body);
                }
            });
            Self {
                url,
                calls,
                stop,
                thread: Some(thread),
            }
        }
        fn normal() -> Self {
            Self::new(|path| match path {
                "/auth/register" => (200, REGISTER.into()),
                "/auth/token" => (200, ACCESS.into()),
                "/ap/exchangetoken/cookies" => (200, COOKIES.into()),
                "/your-orders/orders" => (200, listing()),
                _ => (200, tracking()),
            })
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Release);
            if let Some(thread) = self.thread.take() {
                thread.join().unwrap();
            }
        }
    }
    fn configured(dir: &tempfile::TempDir, fixture: &Fixture) -> AmazonClient {
        let path = dir.path().join("amazon-account.json");
        save(
            &path,
            &Credential {
                refresh_token: "SYNTHETIC_REFRESH".into(),
                device_serial: "SYNTHETIC_SERIAL".into(),
                customer_id: "SYNTHETIC_CUSTOMER".into(),
                generation: 1,
                imports_enabled: false,
                auth_required: false,
                last_success_at: None,
            },
        )
        .unwrap();
        AmazonClient::for_test(path, fixture.url.clone()).unwrap()
    }
    fn compressed_fixture(orders: &'static [u8]) -> Fixture {
        Fixture::with_response(move |path| match path {
            "/auth/token" => (200, String::new(), ACCESS.as_bytes().to_vec()),
            "/ap/exchangetoken/cookies" => (200, String::new(), COOKIES.as_bytes().to_vec()),
            "/your-orders/orders" => (200, "Content-Encoding: gzip\r\n".into(), orders.to_vec()),
            _ => (
                200,
                "Content-Encoding: gzip\r\n".into(),
                include_bytes!("../../tests/fixtures/amazon/tracking.html.gz").to_vec(),
            ),
        })
    }

    #[tokio::test]
    async fn gzip_orders_and_tracking_preview_verified_parcels() {
        let dir = tempfile::tempdir().unwrap();
        let fixture =
            compressed_fixture(include_bytes!("../../tests/fixtures/amazon/orders.html.gz"));
        let client = configured(&dir, &fixture);
        let preview = client.preview().await.unwrap();
        assert_eq!(preview.parcels.len(), 1);
        assert_eq!(preview.deferred, 0);
        assert_eq!(preview.parcels[0].title, "Widget");
        assert_eq!(preview.parcels[0].tracker.tracking_code, NUMBER);
        assert_eq!(
            preview.parcels[0].tracker.status,
            crate::models::Status::InTransit
        );
        assert_eq!(client.status().await.state, AmazonState::Connected);
        assert!(!client.status().await.imports_enabled);
    }

    #[tokio::test]
    async fn gzip_expansion_respects_the_decoded_response_limit() {
        let dir = tempfile::tempdir().unwrap();
        let fixture = compressed_fixture(include_bytes!(
            "../../tests/fixtures/amazon/oversized.html.gz"
        ));
        let client = configured(&dir, &fixture);
        let error = client
            .preview()
            .await
            .err()
            .expect("Oversized preview must fail");
        assert!(matches!(
            error.provider_error().kind,
            ErrorKind::ResponseTooLarge
        ));
        assert_eq!(client.status().await.state, AmazonState::Connected);
        assert!(!client.status().await.imports_enabled);
        let restored = AmazonClient::new(dir.path().join("amazon-account.json")).unwrap();
        assert_eq!(restored.status().await.state, AmazonState::Connected);
    }

    #[tokio::test]
    async fn malformed_gzip_retains_the_saved_connection() {
        let dir = tempfile::tempdir().unwrap();
        let fixture = compressed_fixture(b"invalid compressed response");
        let client = configured(&dir, &fixture);
        assert!(client.preview().await.is_err());
        assert_eq!(client.status().await.state, AmazonState::Connected);
        assert!(!client.status().await.imports_enabled);
        let restored = AmazonClient::new(dir.path().join("amazon-account.json")).unwrap();
        assert_eq!(restored.status().await.state, AmazonState::Connected);
    }

    #[tokio::test]
    async fn sign_in_is_private_owner_bound_and_one_use() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let fixture = Fixture::normal();
        let path = dir.path().join("account.json");
        let client = AmazonClient::for_test(path.clone(), fixture.url.clone()).unwrap();
        let flow = client.begin_sign_in("owner").await.unwrap();
        let landing =
            "https://www.amazon.com/ap/maplanding?openid.oa2.authorization_code=SYNTHETIC_CODE";
        assert!(
            client
                .complete_sign_in("other", &flow.flow_id, landing)
                .await
                .is_err()
        );
        assert!(fixture.calls.lock().unwrap().is_empty());
        let status = client
            .complete_sign_in("owner", &flow.flow_id, landing)
            .await
            .unwrap();
        assert_eq!(status.state, AmazonState::Connected);
        assert!(!status.imports_enabled);
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let public = serde_json::to_string(&status).unwrap();
        for secret in [
            "SYNTHETIC_REFRESH",
            "SYNTHETIC_CUSTOMER",
            "SYNTHETIC_COOKIE",
        ] {
            assert!(!public.contains(secret));
        }
        assert!(
            client
                .complete_sign_in("owner", &flow.flow_id, landing)
                .await
                .is_err()
        );
        let restored = AmazonClient::new(path).unwrap();
        assert_eq!(restored.status().await.state, AmazonState::Connected);
    }
    #[tokio::test]
    async fn expired_flow_and_unsafe_callback_never_reach_network() {
        use crate::amazon::connection::{ConnectionReason, ConnectionStage};
        let dir = tempfile::tempdir().unwrap();
        let fixture = Fixture::normal();
        let client =
            AmazonClient::for_test(dir.path().join("account.json"), fixture.url.clone()).unwrap();
        let flow = client.begin_sign_in("owner").await.unwrap();
        client.state.lock().await.flow.as_mut().unwrap().expires = tokio::time::Instant::now();
        let expired = client
            .complete_sign_in(
                "owner",
                &flow.flow_id,
                "https://www.amazon.com/ap/maplanding?openid.oa2.authorization_code=TEST",
            )
            .await
            .unwrap_err();
        assert_eq!(
            (expired.stage, expired.reason, expired.upstream_status),
            (ConnectionStage::Flow, ConnectionReason::ExpiredFlow, None)
        );
        let unsafe_url = client
            .complete_sign_in(
                "owner",
                &flow.flow_id,
                "https://evil.example/?openid.oa2.authorization_code=TEST",
            )
            .await
            .unwrap_err();
        assert_eq!(
            (
                unsafe_url.stage,
                unsafe_url.reason,
                unsafe_url.upstream_status
            ),
            (
                ConnectionStage::Callback,
                ConnectionReason::InvalidInput,
                None
            )
        );
        assert!(fixture.calls.lock().unwrap().is_empty());
    }
    #[tokio::test]
    async fn preview_does_not_consume_candidates_and_retrieve_reads_one_package() {
        let dir = tempfile::tempdir().unwrap();
        let fixture = Fixture::normal();
        let client = configured(&dir, &fixture);
        let preview = client.discover().await.unwrap();
        assert_eq!(preview.parcels.len(), 1);
        assert!(!client.status().await.imports_enabled);
        client.enable_imports().await.unwrap();
        let actual = client.discover().await.unwrap();
        assert_eq!(actual.parcels.len(), 1);
        assert_eq!(actual.parcels[0].source_key, preview.parcels[0].source_key);
        let tracker = &actual.parcels[0].tracker;
        let before = fixture.calls.lock().unwrap().len();
        let observed = client.retrieve(&tracker.id, NUMBER).await.unwrap();
        assert_eq!(observed.tracking_code, NUMBER);
        assert_eq!(fixture.calls.lock().unwrap().len(), before + 1);
        assert!(
            fixture
                .calls
                .lock()
                .unwrap()
                .last()
                .unwrap()
                .starts_with("/gp/your-account/ship-track?")
        );
        client
            .remember_imported(&[actual.parcels[0].source_key.clone()])
            .await;
        let after = client.discover().await.unwrap();
        assert!(after.parcels.is_empty());
    }
    #[tokio::test]
    async fn confirmed_revocation_is_sticky_and_outages_are_not_auth_failures() {
        for (status, body, expired) in [
            (400, r#"{"error":"invalid_grant"}"#, true),
            (403, r#"{"message":"Access denied"}"#, false),
            (503, "Unavailable", false),
            (200, "CAPTCHA", false),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let fixture = Fixture::new(move |_| (status, body.into()));
            let client = configured(&dir, &fixture);
            assert!(client.discover().await.is_err());
            assert_eq!(
                client.status().await.state,
                if expired {
                    AmazonState::AuthRequired
                } else {
                    AmazonState::Connected
                }
            );
            if expired {
                let count = fixture.calls.lock().unwrap().len();
                assert!(client.discover().await.is_err());
                assert_eq!(fixture.calls.lock().unwrap().len(), count);
                let restored = AmazonClient::new(dir.path().join("amazon-account.json")).unwrap();
                assert_eq!(restored.status().await.state, AmazonState::AuthRequired);
            }
        }
    }

    #[tokio::test]
    async fn discovery_checks_revocation_even_while_website_cookies_still_work() {
        let refreshes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count = refreshes.clone();
        let fixture = Fixture::new(move |path| match path {
            "/auth/token" if count.fetch_add(1, Ordering::AcqRel) == 0 => (200, ACCESS.into()),
            "/auth/token" => (400, r#"{"error":"invalid_grant"}"#.into()),
            "/ap/exchangetoken/cookies" => (200, COOKIES.into()),
            "/your-orders/orders" => (200, listing()),
            _ => (200, tracking()),
        });
        let dir = tempfile::tempdir().unwrap();
        let client = configured(&dir, &fixture);
        assert_eq!(client.discover().await.unwrap().parcels.len(), 1);
        let error = match client.discover().await {
            Ok(_) => panic!("revoked token was accepted"),
            Err(error) => error,
        };
        assert!(matches!(error.kind, ErrorKind::AuthenticationRequired));
        assert_eq!(client.status().await.state, AmazonState::AuthRequired);
        let calls = fixture.calls.lock().unwrap();
        assert_eq!(
            calls
                .iter()
                .filter(|path| path.as_str() == "/auth/token")
                .count(),
            2
        );
        assert_eq!(
            calls
                .iter()
                .filter(|path| path.as_str() == "/your-orders/orders")
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn disconnect_invalidates_import_generation_and_old_account_paths() {
        let dir = tempfile::tempdir().unwrap();
        let fixture = Fixture::normal();
        let client = configured(&dir, &fixture);
        client.enable_imports().await.unwrap();
        let discovery = client.discover().await.unwrap();
        assert!(client.commit_guard(discovery.generation).await.is_ok());
        client.disconnect().await.unwrap();
        assert!(client.commit_guard(discovery.generation).await.is_err());
        assert_eq!(client.status().await.state, AmazonState::Disconnected);
        let client = configured(&dir, &fixture);
        let bad = format!("{}:{PATH}", account_key("OTHER_ACCOUNT"));
        let before = fixture.calls.lock().unwrap().len();
        assert!(client.retrieve(&bad, NUMBER).await.is_err());
        assert_eq!(fixture.calls.lock().unwrap().len(), before);
    }
    #[tokio::test]
    async fn unknown_registration_cannot_claim_authoritative_absence() {
        let dir = tempfile::tempdir().unwrap();
        let fixture = Fixture::normal();
        let client = configured(&dir, &fixture);
        let request = crate::shipping_api::types::Registration {
            tracking_number: NUMBER.into(),
            carrier: None,
            recipient_url: None,
            started_at: 0,
        };
        let error = client.reconcile_registration(&request).await.unwrap_err();
        assert!(matches!(error.kind, ErrorKind::RegistrationAmbiguous));
        assert!(fixture.calls.lock().unwrap().is_empty());
    }
    #[test]
    fn credential_permissions_and_fixture_hosts_are_checked() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("account.json");
        std::fs::write(&path, "{}").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(AmazonClient::new(path.clone()).is_err());
        for url in [
            "https://www.amazon.com/",
            "http://localhost/",
            "http://127.0.0.1.evil.example/",
        ] {
            assert!(
                AmazonClient::for_test(path.clone(), reqwest::Url::parse(url).unwrap()).is_err()
            );
        }
    }
    #[tokio::test]
    async fn discovery_rotates_bounded_work_without_consuming_preview() {
        let listing = (0..15).map(|i| format!(r#"<a class="item-card__link" href="/gp/your-account/ship-track?orderId=111-1234567-1234567&amp;shipmentId=SHIP{i}&amp;packageId=1" aria-label="Widget {i}, Order placed 1 October 2026, Shipped"></a>"#)).collect::<String>();
        let fixture = Fixture::new(move |path| match path {
            "/auth/token" => (200, ACCESS.into()),
            "/ap/exchangetoken/cookies" => (200, COOKIES.into()),
            "/your-orders/orders" => (200, listing.clone()),
            _ => (200, tracking()),
        });
        let dir = tempfile::tempdir().unwrap();
        let client = configured(&dir, &fixture);
        let first = client.discover().await.unwrap();
        assert_eq!(first.parcels.len(), 10);
        assert_eq!(first.deferred, 5);
        let first_keys = first
            .parcels
            .iter()
            .map(|p| p.source_key.clone())
            .collect::<std::collections::BTreeSet<_>>();
        let second = client.discover().await.unwrap();
        assert_eq!(second.parcels.len(), 10);
        assert!(
            second
                .parcels
                .iter()
                .any(|p| !first_keys.contains(&p.source_key))
        );
        client
            .remember_imported(&first_keys.into_iter().collect::<Vec<_>>())
            .await;
        let remaining = client.discover().await.unwrap();
        assert_eq!(remaining.parcels.len(), 5);
    }

    #[tokio::test]
    async fn observation_guard_rejects_old_scope_but_keeps_same_epoch_auth_error() {
        let fixture = Fixture::normal();
        let dir = tempfile::tempdir().unwrap();
        let client = configured(&dir, &fixture);
        client.enable_imports().await.unwrap();
        let scoped = client.for_request("test", None);
        let tracker = format!("{}:{PATH}", account_key("SYNTHETIC_CUSTOMER"));
        scoped.retrieve(&tracker, NUMBER).await.unwrap();
        client.disconnect().await.unwrap();
        assert!(scoped.observation_guard().await.is_err());
        let fixture = Fixture::new(|_| (400, r#"{"error":"invalid_grant"}"#.into()));
        let client = configured(&dir, &fixture);
        client.enable_imports().await.unwrap();
        let scoped = client.for_request("test", None);
        assert!(matches!(
            scoped.retrieve(&tracker, NUMBER).await.unwrap_err().kind,
            ErrorKind::AuthenticationRequired
        ));
        assert!(scoped.observation_guard().await.is_ok());
    }
    #[tokio::test]
    async fn preview_then_shared_discovery_worker_imports_once_and_respects_due_time() {
        use crate::{amazon_sync::AmazonDiscoveryWorker, store::Store};
        let dir = tempfile::tempdir().unwrap();
        let fixture = Fixture::normal();
        let client = Arc::new(configured(&dir, &fixture));
        assert_eq!(client.discover().await.unwrap().parcels.len(), 1);
        client.enable_imports().await.unwrap();
        let store = Store::open(&dir.path().join("shipments.sqlite3"))
            .await
            .unwrap();
        let worker = AmazonDiscoveryWorker::new(store.clone(), client.clone());
        assert!(worker.tick().await.unwrap());
        let id: String = sqlx::query_scalar("SELECT id FROM shipments WHERE tracking_number=?")
            .bind(NUMBER)
            .fetch_one(store.pool())
            .await
            .unwrap();
        let shipment = store
            .get(uuid::Uuid::parse_str(&id).unwrap())
            .await
            .unwrap();
        assert_eq!(shipment.title, "Widget");
        assert_eq!(
            shipment.sync.provider,
            crate::shipping_api::types::ProviderId::AmazonAccount
        );
        assert_eq!(shipment.tracking.carrier, crate::carrier::Carrier::Usps);
        assert_eq!(shipment.sources.len(), 1);
        let before = fixture.calls.lock().unwrap().len();
        assert!(!worker.tick().await.unwrap());
        assert_eq!(fixture.calls.lock().unwrap().len(), before);
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM shipments")
                .fetch_one(store.pool())
                .await
                .unwrap(),
            1
        );
        store.close().await.unwrap();
    }

    #[tokio::test]
    async fn shared_shipment_worker_reads_only_saved_page_and_uses_status_cadence() {
        use crate::{amazon_sync::AmazonDiscoveryWorker, store::Store, sync::SyncWorker};
        for (status, low, high) in [("IN_TRANSIT", 1800, 1980), ("OUT_FOR_DELIVERY", 300, 330)] {
            let fixture = Fixture::new(move |path| match path {
                "/auth/token" => (200, ACCESS.into()),
                "/ap/exchangetoken/cookies" => (200, COOKIES.into()),
                "/your-orders/orders" => (200, listing()),
                _ => (200, tracking().replace("IN_TRANSIT", status)),
            });
            let dir = tempfile::tempdir().unwrap();
            let client = Arc::new(configured(&dir, &fixture));
            client.enable_imports().await.unwrap();
            let store = Store::open(&dir.path().join("shipments.sqlite3"))
                .await
                .unwrap();
            assert!(
                AmazonDiscoveryWorker::new(store.clone(), client.clone())
                    .tick()
                    .await
                    .unwrap()
            );
            sqlx::query("UPDATE shipments SET next_attempt_at=unixepoch()")
                .execute(store.pool())
                .await
                .unwrap();
            let before = fixture.calls.lock().unwrap().len();
            assert!(
                SyncWorker::new(store.pool().clone(), client.clone())
                    .tick()
                    .await
                    .unwrap()
            );
            let saved: (Option<String>, Option<String>, Option<String>) =
                sqlx::query_as("SELECT tracker_id,last_error,last_error_code FROM shipments")
                    .fetch_one(store.pool())
                    .await
                    .unwrap();
            let call_count = fixture.calls.lock().unwrap().len();
            let connection = client.status().await;
            assert_eq!(
                call_count,
                before + 1,
                "saved: {saved:?}; status: {connection:?}"
            );
            assert!(
                fixture
                    .calls
                    .lock()
                    .unwrap()
                    .last()
                    .unwrap()
                    .starts_with("/gp/your-account/ship-track?")
            );
            let delay: i64 =
                sqlx::query_scalar("SELECT next_attempt_at-unixepoch() FROM shipments")
                    .fetch_one(store.pool())
                    .await
                    .unwrap();
            assert!((low - 1..=high).contains(&delay), "{status}: {delay}");
            let errors: i64 =
                sqlx::query_scalar("SELECT count(*) FROM shipments WHERE last_error IS NOT NULL")
                    .fetch_one(store.pool())
                    .await
                    .unwrap();
            assert_eq!(errors, 0);
            store.close().await.unwrap();
        }
    }
    #[tokio::test]
    async fn redirects_cannot_assign_another_packages_tracking_to_the_requested_title() {
        for (destination, accepted) in [
            (PATH.replace("packageId=1", "packageId=2"), false),
            (
                PATH.replace("/ship-track?", "/ship-track/ref=tracking_ref?"),
                true,
            ),
        ] {
            let target = destination.clone();
            let redirected = AtomicBool::new(false);
            let fixture = Fixture::with_location(move |path| match path {
                "/auth/token" => (200, None, ACCESS.into()),
                "/ap/exchangetoken/cookies" => (200, None, COOKIES.into()),
                "/your-orders/orders" => (200, None, listing()),
                value if value == PATH && !redirected.swap(true, Ordering::AcqRel) => {
                    (302, Some(target.clone()), String::new())
                }
                _ => (200, None, tracking()),
            });
            let dir = tempfile::tempdir().unwrap();
            let client = configured(&dir, &fixture);
            if accepted {
                let result = client.discover().await.unwrap();
                assert_eq!(result.parcels.len(), 1);
                assert_eq!(result.parcels[0].title, "Widget");
                assert_eq!(result.parcels[0].tracker.tracking_code, NUMBER);
                assert_eq!(result.deferred, 0);
            } else {
                let result = client.discover().await.unwrap();
                assert!(result.parcels.is_empty());
                assert_eq!(result.deferred, 1);
                assert!(
                    !fixture
                        .calls
                        .lock()
                        .unwrap()
                        .iter()
                        .any(|path| path.contains("packageId=2"))
                );
            }
        }
    }
    #[tokio::test]
    async fn empty_and_missing_cookie_paths_complete_real_connection_flow() {
        for cookies in [
            COOKIES.replace("\"Path\":\"/\"", "\"Path\":\"\""),
            COOKIES.replace(",\"Path\":\"/\"", ""),
        ] {
            let fixture = Fixture::new(move |path| match path {
                "/auth/register" => (200, REGISTER.into()),
                "/auth/token" => (200, ACCESS.into()),
                "/ap/exchangetoken/cookies" => (200, cookies.clone()),
                "/your-orders/orders" => (200, listing()),
                _ => (200, tracking()),
            });
            let dir = tempfile::tempdir().unwrap();
            let client =
                AmazonClient::for_test(dir.path().join("account.json"), fixture.url.clone())
                    .unwrap();
            let flow = client.begin_sign_in("owner").await.unwrap();
            let result = client.complete_sign_in("owner", &flow.flow_id,
                "https://www.amazon.com/ap/maplanding?openid.oa2.authorization_code=SYNTHETIC_CODE").await;
            let status = result.unwrap();
            assert_eq!(status.state, AmazonState::Connected);
            assert!(!status.imports_enabled);
            assert_eq!(fixture.calls.lock().unwrap().len(), 2);
            assert_eq!(client.preview().await.unwrap().parcels.len(), 1);
        }
    }

    #[tokio::test]
    async fn connection_failures_identify_the_boundary_without_saving_or_reusing_a_flow() {
        use crate::amazon::connection::{ConnectionReason, ConnectionStage};
        let cases = [
            (
                "/auth/register",
                200,
                r#"{"response":{"success":{"tokens":{"bearer":{"refresh_token":"PRIVATE_TOKEN"}}}}}"#,
                ConnectionStage::Registration,
                ConnectionReason::AccountIdentityMissing,
            ),
            (
                "/auth/register",
                401,
                r#"{"error":"invalid_grant","message":"PRIVATE_BODY"}"#,
                ConnectionStage::Registration,
                ConnectionReason::UpstreamRejected,
            ),
            (
                "/auth/register",
                500,
                "PRIVATE_BODY",
                ConnectionStage::Registration,
                ConnectionReason::UpstreamHttp,
            ),
            (
                "/auth/token",
                200,
                r#"{"unexpected":"PRIVATE_TOKEN"}"#,
                ConnectionStage::AccessToken,
                ConnectionReason::UnexpectedResponse,
            ),
        ];
        for (failed_path, status, body, stage, reason) in cases {
            let fixture = Fixture::new(move |path| {
                if path == failed_path {
                    return (status, body.into());
                }
                match path {
                    "/auth/register" => (200, REGISTER.into()),
                    "/auth/token" => (200, ACCESS.into()),
                    "/ap/exchangetoken/cookies" => (200, COOKIES.into()),
                    "/your-orders/orders" => (200, listing()),
                    _ => (404, String::new()),
                }
            });
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("account.json");
            let client = AmazonClient::for_test(path.clone(), fixture.url.clone()).unwrap();
            let flow = client.begin_sign_in("owner").await.unwrap();
            let callback =
                "https://www.amazon.com/ap/maplanding?openid.oa2.authorization_code=PRIVATE_CODE";
            let error = client
                .complete_sign_in("owner", &flow.flow_id, callback)
                .await
                .unwrap_err();
            assert_eq!(
                (error.stage, error.reason, error.upstream_status),
                (stage, reason, Some(status))
            );
            let diagnostic = format!("{error:?} {error}");
            assert!(!diagnostic.contains("PRIVATE_"));
            let state = client.status().await;
            assert_eq!(state.state, AmazonState::Disconnected);
            assert!(!state.imports_enabled);
            assert_eq!(state.last_error_code, Some(error.code()));
            assert!(!path.exists());
            let requests = fixture.calls.lock().unwrap().len();
            let retry = client
                .complete_sign_in("owner", &flow.flow_id, callback)
                .await
                .unwrap_err();
            assert_eq!(retry.stage, ConnectionStage::Flow);
            assert_eq!(fixture.calls.lock().unwrap().len(), requests);
        }
    }

    #[tokio::test]
    async fn failed_credential_write_has_an_explicit_local_failure() {
        use crate::amazon::connection::{ConnectionReason, ConnectionStage};
        let dir = tempfile::tempdir().unwrap();
        let fixture = Fixture::normal();
        let path = dir.path().join("account.json");
        let client = AmazonClient::for_test(path.clone(), fixture.url.clone()).unwrap();
        let flow = client.begin_sign_in("owner").await.unwrap();
        std::fs::create_dir(&path).unwrap();
        let error = client
            .complete_sign_in(
                "owner",
                &flow.flow_id,
                "https://www.amazon.com/ap/maplanding?openid.oa2.authorization_code=SYNTHETIC_CODE",
            )
            .await
            .unwrap_err();
        assert_eq!(
            (error.stage, error.reason, error.upstream_status),
            (
                ConnectionStage::Credentials,
                ConnectionReason::Persistence,
                None
            )
        );
        assert_eq!(client.status().await.state, AmazonState::Disconnected);
        assert!(path.is_dir());
    }
    #[tokio::test]
    async fn a_different_account_never_replaces_saved_credentials_or_import_settings() {
        use crate::amazon::connection::{ConnectionReason, ConnectionStage};
        let fixture = Fixture::new(|path| {
            assert_eq!(path, "/auth/register");
            (
                200,
                REGISTER.replace("SYNTHETIC_CUSTOMER", "OTHER_CUSTOMER"),
            )
        });
        let dir = tempfile::tempdir().unwrap();
        let client = configured(&dir, &fixture);
        client.enable_imports().await.unwrap();
        let path = dir.path().join("amazon-account.json");
        let original = std::fs::read(&path).unwrap();
        let flow = client.begin_sign_in("owner").await.unwrap();
        let error = client
            .complete_sign_in(
                "owner",
                &flow.flow_id,
                "https://www.amazon.com/ap/maplanding?openid.oa2.authorization_code=SYNTHETIC_CODE",
            )
            .await
            .unwrap_err();
        assert_eq!(
            (error.stage, error.reason),
            (ConnectionStage::Account, ConnectionReason::AccountMismatch)
        );
        assert!(
            std::fs::read(&path).unwrap() == original,
            "Existing credential file changed"
        );
        let status = client.status().await;
        assert_eq!(status.state, AmazonState::Connected);
        assert!(status.imports_enabled);
        assert_eq!(status.last_error_code, None);
        assert_eq!(fixture.calls.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_superseded_connection_failure_cannot_poison_a_newly_saved_connection() {
        use crate::amazon::connection::{ConnectionReason, ConnectionStage};
        let held = Arc::new(AtomicBool::new(false));
        let entered = held.clone();
        let (release, wait) = std::sync::mpsc::channel();
        let wait = Mutex::new(Some(wait));
        let fixture = Fixture::new(move |path| {
            if path == "/auth/token"
                && let Some(wait) = wait.lock().unwrap().take()
            {
                entered.store(true, Ordering::Release);
                wait.recv_timeout(std::time::Duration::from_secs(5))
                    .unwrap();
            }
            match path {
                "/auth/register" => (200, REGISTER.into()),
                "/auth/token" => (200, ACCESS.into()),
                "/ap/exchangetoken/cookies" => (200, COOKIES.into()),
                "/your-orders/orders" => (200, listing()),
                _ => (404, String::new()),
            }
        });
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("account.json");
        let client = AmazonClient::for_test(path.clone(), fixture.url.clone()).unwrap();
        let first = client.begin_sign_in("owner").await.unwrap();
        let a = client.clone();
        let first = tokio::spawn(complete_fixture_connection(a, first.flow_id));
        for _ in 0..100 {
            if held.load(Ordering::Acquire) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert!(held.load(Ordering::Acquire));
        let second = client.begin_sign_in("owner").await.unwrap();
        let b = client.clone();
        let second = tokio::spawn(complete_fixture_connection(b, second.flow_id));
        for _ in 0..100 {
            if client.state.lock().await.flow.is_none() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert!(client.state.lock().await.flow.is_none());
        release.send(()).unwrap();
        first.await.unwrap().unwrap();
        let saved = std::fs::read(&path).unwrap();
        let error = second.await.unwrap().unwrap_err();
        assert_eq!(
            (error.stage, error.reason),
            (ConnectionStage::Account, ConnectionReason::SessionChanged)
        );
        assert!(
            std::fs::read(&path).unwrap() == saved,
            "Saved credential file changed"
        );
        let status = client.status().await;
        assert_eq!(status.state, AmazonState::Connected);
        assert_eq!(status.last_error_code, None);
    }

    async fn complete_fixture_connection(
        client: AmazonClient,
        flow: String,
    ) -> Result<crate::amazon::AmazonStatus, crate::amazon::connection::ConnectionError> {
        client
            .complete_sign_in(
                "owner",
                &flow,
                "https://www.amazon.com/ap/maplanding?openid.oa2.authorization_code=SYNTHETIC_CODE",
            )
            .await
    }
    #[tokio::test]
    async fn verified_sign_in_is_durable_without_fetching_orders() {
        let fixture = Fixture::new(|path| match path {
            "/auth/register" => (200, REGISTER.into()),
            "/auth/token" => (200, ACCESS.into()),
            "/ap/exchangetoken/cookies" => (200, COOKIES.into()),
            _ => (200, "<html>Unrecognized orders layout</html>".into()),
        });
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("account.json");
        let client = AmazonClient::for_test(path.clone(), fixture.url.clone()).unwrap();
        let flow = client.begin_sign_in("owner").await.unwrap();
        let status = client
            .complete_sign_in(
                "owner",
                &flow.flow_id,
                "https://www.amazon.com/ap/maplanding?openid.oa2.authorization_code=SYNTHETIC_CODE",
            )
            .await
            .unwrap();
        assert_eq!(status.state, AmazonState::Connected);
        assert!(!status.imports_enabled);
        assert_eq!(
            AmazonClient::new(path).unwrap().status().await.state,
            AmazonState::Connected
        );
        assert_eq!(
            *fixture.calls.lock().unwrap(),
            vec!["/auth/register", "/auth/token"]
        );
    }

    #[tokio::test]
    async fn legacy_orders_fallback_is_bounded_and_unknown_pages_keep_the_saved_grant() {
        use crate::amazon::connection::ConnectionReason;
        for recognized in [true, false] {
            let fixture = Fixture::new(move |path| match path {
                "/auth/token" => (200, ACCESS.into()),
                "/ap/exchangetoken/cookies" => (200, COOKIES.into()),
                "/your-orders/orders" => (200, "<html>Unknown first layout</html>".into()),
                "/gp/css/order-history" if recognized => (200, listing()),
                "/gp/css/order-history" => (200, "<html>Unknown legacy layout</html>".into()),
                _ => (200, tracking()),
            });
            let dir = tempfile::tempdir().unwrap();
            let client = configured(&dir, &fixture);
            let path = dir.path().join("amazon-account.json");
            let before = std::fs::read(&path).unwrap();
            let result = client.preview().await;
            if recognized {
                assert_eq!(result.unwrap().parcels.len(), 1);
            } else {
                let error = result.err().unwrap();
                assert_eq!(error.reason, ConnectionReason::UnsupportedOrdersLayout);
                assert_eq!(error.response_shape, Some("unrecognized_html"));
                assert_eq!(error.upstream_status, Some(200));
                assert!(
                    std::fs::read(&path).unwrap() == before,
                    "A layout failure changed saved credentials"
                );
                assert_eq!(
                    AmazonClient::new(path).unwrap().status().await.state,
                    AmazonState::Connected
                );
            }
            assert_eq!(client.status().await.state, AmazonState::Connected);
            assert!(!client.status().await.imports_enabled);
            let calls = fixture.calls.lock().unwrap();
            assert_eq!(
                calls
                    .iter()
                    .filter(|p| p.as_str() == "/gp/css/order-history")
                    .count(),
                1
            );
            assert!(!calls.iter().any(|p| p == "/auth/register"));
        }
    }

    #[tokio::test]
    async fn outages_and_challenges_do_not_become_empty_orders_or_expired_credentials() {
        use crate::amazon::connection::ConnectionReason;
        for (status, body, reason) in [
            (503, "outage", ConnectionReason::UpstreamHttp),
            (429, "limited", ConnectionReason::UpstreamHttp),
            (500, "outage", ConnectionReason::UpstreamHttp),
            (
                200,
                "<input id=captchacharacters>",
                ConnectionReason::ChallengePage,
            ),
        ] {
            let fixture = Fixture::new(move |path| match path {
                "/auth/token" => (200, ACCESS.into()),
                "/ap/exchangetoken/cookies" => (200, COOKIES.into()),
                _ => (status, body.into()),
            });
            let dir = tempfile::tempdir().unwrap();
            let client = configured(&dir, &fixture);
            let error = client.preview().await.err().unwrap();
            assert_eq!(
                error.reason,
                if status == 429 {
                    ConnectionReason::RateLimited
                } else {
                    reason
                }
            );
            assert_eq!(client.status().await.state, AmazonState::Connected);
            assert!(
                !fixture
                    .calls
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|p| p == "/gp/css/order-history")
            );
        }
    }

    #[tokio::test]
    async fn refresh_rotation_survives_a_cookie_outage_and_reconstruction() {
        use crate::amazon::connection::ConnectionStage;
        let fixture = Fixture::new(|path| match path {
            "/auth/token" => (
                200,
                r#"{"access_token":"SYNTHETIC_ACCESS","refresh_token":"SYNTHETIC_ROTATED"}"#.into(),
            ),
            "/ap/exchangetoken/cookies" => (503, "outage".into()),
            _ => panic!("Unexpected synthetic request"),
        });
        let dir = tempfile::tempdir().unwrap();
        let client = configured(&dir, &fixture);
        let error = client.preview().await.err().unwrap();
        assert_eq!(error.stage, ConnectionStage::Cookies);
        let restored = AmazonClient::new(dir.path().join("amazon-account.json")).unwrap();
        let state = restored.state.lock().await;
        assert!(
            state.credential.as_ref().unwrap().refresh_token == "SYNTHETIC_ROTATED",
            "Replacement refresh token was not persisted"
        );
        assert!(!state.credential.as_ref().unwrap().auth_required);
        assert!(!state.credential.as_ref().unwrap().imports_enabled);
        assert_eq!(
            *fixture.calls.lock().unwrap(),
            vec!["/auth/token", "/ap/exchangetoken/cookies"]
        );
    }
}
