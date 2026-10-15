use crate::{
    installation::{Installation, InstallationReport, IntegrationPresence, valid_presence},
    models::*,
    store::{Store, validate_create, validate_update},
    sync::EasyPostClient,
};
use anyhow::{Context as _, Result, anyhow};
use dropshot::ClientErrorStatusCode as StatusCode;
use dropshot::{
    ApiDescription, Body, ConfigDropshot, HttpError, HttpResponseCreated, HttpResponseHeaders,
    HttpResponseOk, HttpResponseUpdatedNoContent, HttpServer, HttpServerStarter, PaginationParams,
    Path, Query, RequestContext, ResultsPage, TypedBody, WhichPage, endpoint,
};
use http::{HeaderValue, Response};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    net::{IpAddr, SocketAddr},
    sync::Arc,
};
use subtle::ConstantTimeEq;
use uuid::Uuid;

#[derive(Clone)]
pub struct Context {
    pub store: Store,
    owner_digest: [u8; 32],
    integration_digest: Option<[u8; 32]>,
    ingress_peer: Option<IpAddr>,
    provider: Option<EasyPostClient>,
    installation: Arc<Installation>,
}

impl Context {
    pub fn new(
        store: Store,
        owner: &str,
        ingress_peer: Option<IpAddr>,
        provider: Option<EasyPostClient>,
    ) -> Self {
        Self {
            store,
            owner_digest: Sha256::digest(owner).into(),
            integration_digest: None,
            ingress_peer,
            provider,
            installation: Arc::new(Installation::new(false)),
        }
    }

    pub fn with_integration(mut self, token: &str) -> Self {
        self.integration_digest = Some(Sha256::digest(token).into());
        self.installation = Arc::new(Installation::new(true));
        self
    }

    pub fn for_ingress(mut self, peer: IpAddr) -> Self {
        self.ingress_peer = Some(peer);
        self
    }
}

struct Principal {
    actor: String,
    owner: bool,
    integration: bool,
}

fn client_error(status: StatusCode, message: &str) -> HttpError {
    HttpError::for_client_error(None, status, message.into())
}

fn header<'a>(request: &'a RequestContext<Arc<Context>>, name: &str) -> Option<&'a str> {
    request
        .request
        .headers()
        .get(name)
        .and_then(|value| value.to_str().ok())
}

fn ingress_access(request: &RequestContext<Arc<Context>>) -> Result<(), HttpError> {
    if let Some(peer) = request.context().ingress_peer
        && request.request.remote_addr().ip() != peer
    {
        return Err(client_error(
            StatusCode::FORBIDDEN,
            "Ingress accepts only the configured Supervisor peer",
        ));
    }
    Ok(())
}

async fn authenticate(
    request: &RequestContext<Arc<Context>>,
    human_only: bool,
) -> Result<Principal, HttpError> {
    let context = request.context();
    let principal = if context.ingress_peer.is_some() {
        ingress_access(request)?;
        if request.request.method() != http::Method::GET
            && header(request, "x-shipments-request") != Some("1")
        {
            return Err(client_error(
                StatusCode::FORBIDDEN,
                "missing browser request header",
            ));
        }
        let user = header(request, "x-remote-user-id")
            .filter(|value| !value.is_empty())
            .ok_or_else(|| client_error(StatusCode::UNAUTHORIZED, "missing Home Assistant user"))?;
        Principal {
            actor: format!("ha:{user}"),
            owner: true,
            integration: false,
        }
    } else {
        let token = header(request, "authorization")
            .and_then(|value| value.strip_prefix("Bearer "))
            .ok_or_else(|| {
                client_error(StatusCode::UNAUTHORIZED, "Bearer authentication required")
            })?;
        let digest: [u8; 32] = Sha256::digest(token).into();
        if bool::from(digest.ct_eq(&context.owner_digest)) {
            let actor = header(request, "x-shipments-actor").unwrap_or("owner");
            if actor.len() > 200 || actor.is_empty() || actor.chars().any(char::is_control) {
                return Err(client_error(
                    StatusCode::BAD_REQUEST,
                    "invalid actor header",
                ));
            }
            Principal {
                actor: actor.to_string(),
                owner: true,
                integration: false,
            }
        } else if context
            .integration_digest
            .as_ref()
            .is_some_and(|expected| bool::from(digest.ct_eq(expected)))
        {
            let actor = header(request, "x-shipments-actor");
            if request.request.method() != http::Method::GET
                && !actor.is_some_and(|value| {
                    value.starts_with("ha:")
                        && value.len() > 3
                        && value.len() <= 200
                        && !value.chars().any(char::is_control)
                })
            {
                return Err(client_error(
                    StatusCode::FORBIDDEN,
                    "integration writes require a Home Assistant actor",
                ));
            }
            Principal {
                actor: actor.unwrap_or("ha:integration").into(),
                owner: false,
                integration: true,
            }
        } else if let Some(id) = context.store.agent_identity(&digest).await? {
            Principal {
                actor: format!("agent:{id}"),
                owner: false,
                integration: false,
            }
        } else {
            return Err(client_error(StatusCode::UNAUTHORIZED, "invalid credential"));
        }
    };
    if human_only && !principal.owner && !principal.integration {
        return Err(client_error(
            StatusCode::FORBIDDEN,
            "owner credential required for manual corrections and archive",
        ));
    }
    Ok(principal)
}

fn revision(request: &RequestContext<Arc<Context>>) -> Result<i64, HttpError> {
    let value = header(request, "if-match").ok_or_else(|| {
        client_error(
            StatusCode::PRECONDITION_REQUIRED,
            "If-Match is required; use the ETag from the current shipment",
        )
    })?;
    let value = value
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
        .and_then(|value| value.parse::<i64>().ok())
        .filter(|value| *value > 0)
        .ok_or_else(|| {
            client_error(
                StatusCode::BAD_REQUEST,
                "If-Match must be a single quoted revision",
            )
        })?;
    Ok(value)
}

#[derive(Serialize, JsonSchema)]
struct VersionHeaders {
    etag: String,
}

#[derive(Serialize, JsonSchema)]
struct PrivateHeaders {
    #[serde(rename = "cache-control")]
    cache_control: String,
}

fn private_headers() -> PrivateHeaders {
    PrivateHeaders {
        cache_control: "no-store".into(),
    }
}

type ShipmentResponse = HttpResponseHeaders<HttpResponseOk<Shipment>, VersionHeaders>;
fn shipment_response(shipment: Shipment) -> ShipmentResponse {
    let headers = VersionHeaders {
        etag: format!("\"{}\"", shipment.revision),
    };
    let mut response = HttpResponseHeaders::new(HttpResponseOk(shipment), headers);
    response.headers_mut().insert(
        http::header::CACHE_CONTROL,
        HeaderValue::from_static("no-store"),
    );
    response
}

#[derive(Deserialize, JsonSchema)]
struct ShipmentPath {
    id: Uuid,
}

#[derive(Deserialize, Serialize, JsonSchema)]
struct ShipmentCursor {
    after: Uuid,
    #[serde(default)]
    sort_key: Option<i64>,
    filters: Filters,
}

/// List local shipment records using stable keyset pagination.
#[endpoint {method=GET,path="/v1/shipments",tags=["shipments"]}]
async fn shipments_list(
    request: RequestContext<Arc<Context>>,
    query: Query<PaginationParams<Filters, ShipmentCursor>>,
) -> Result<HttpResponseOk<ResultsPage<Shipment>>, HttpError> {
    authenticate(&request, false).await?;
    let params = query.into_inner();
    let limit = request.page_limit(&params)?.get().min(200);
    let (filters, after) = match &params.page {
        WhichPage::First(filters) => (filters.clone(), None),
        WhichPage::Next(cursor) => (
            cursor.filters.clone(),
            Some(ShipmentPosition {
                id: cursor.after,
                sort_key: cursor.sort_key,
            }),
        ),
    };
    let mut items = request
        .context()
        .store
        .list(&filters, after, limit + 1)
        .await?;
    let more = items.len() > limit as usize;
    items.truncate(limit as usize);
    let mut page = ResultsPage::new(items, &filters, |item, filters| ShipmentCursor {
        after: item.position.id,
        sort_key: item.position.sort_key,
        filters: filters.clone(),
    })?;
    if !more {
        page.next_page = None;
    }
    Ok(HttpResponseOk(ResultsPage {
        items: page.items.into_iter().map(|entry| entry.shipment).collect(),
        next_page: page.next_page,
    }))
}

/// List EasyPost's canonical tracking carriers. Omit carrier when automatic detection is preferred.
#[endpoint {method=GET,path="/v1/carriers",tags=["shipments"]}]
async fn carriers_list(
    request: RequestContext<Arc<Context>>,
) -> Result<HttpResponseOk<Vec<crate::carrier::Carrier>>, HttpError> {
    authenticate(&request, false).await?;
    Ok(HttpResponseOk(crate::carrier::Carrier::ALL.to_vec()))
}

/// Save a durable pending registration before contacting the tracking provider.
#[endpoint {method=POST,path="/v1/shipments",tags=["shipments"]}]
async fn shipments_create(
    request: RequestContext<Arc<Context>>,
    body: TypedBody<CreateShipment>,
) -> Result<HttpResponseHeaders<HttpResponseCreated<Shipment>, VersionHeaders>, HttpError> {
    let principal = authenticate(&request, false).await?;
    let key = header(&request, "idempotency-key")
        .ok_or_else(|| client_error(StatusCode::BAD_REQUEST, "Idempotency-Key is required"))?;
    let mut input = body.into_inner();
    validate_create(&mut input, key)
        .map_err(|error| log_validation_error(&request, error, "create"))?;
    validate_admission(&input.tracking)
        .map_err(|error| log_validation_error(&request, error, "create"))?;
    let store = &request.context().store;
    let shipment = if let Some(saved) = store.replay_create(&input, &principal.actor, key).await? {
        saved
    } else {
        store.create(input, &principal.actor, key, None).await?
    };
    slog::info!(request.log, "tracking_registration";
        "request_id" => request.request_id.clone(), "shipment_id" => shipment.id.to_string(),
        "operation" => "create", "outcome" => "saved");
    let headers = VersionHeaders {
        etag: format!("\"{}\"", shipment.revision),
    };
    Ok(HttpResponseHeaders::new(
        HttpResponseCreated(shipment),
        headers,
    ))
}

#[endpoint {method=GET,path="/v1/shipments/{id}",tags=["shipments"]}]
async fn shipment_get(
    request: RequestContext<Arc<Context>>,
    path: Path<ShipmentPath>,
) -> Result<ShipmentResponse, HttpError> {
    authenticate(&request, false).await?;
    Ok(shipment_response(
        request.context().store.get(path.into_inner().id).await?,
    ))
}

#[endpoint {method=PATCH,path="/v1/shipments/{id}",tags=["shipments"]}]
async fn shipment_update(
    request: RequestContext<Arc<Context>>,
    path: Path<ShipmentPath>,
    body: TypedBody<UpdateShipment>,
) -> Result<ShipmentResponse, HttpError> {
    let principal = authenticate(&request, false).await?;
    let id = path.into_inner().id;
    let expected_revision = revision(&request)?;
    let mut input = body.into_inner();
    validate_update(&mut input).map_err(|error| log_validation_error(&request, error, "update"))?;
    if let Some(value) = &mut input.tracking.value {
        let tracking = value
            .as_mut()
            .ok_or(crate::error::Error::TrackingRequired)?;
        let current = request.context().store.get(id).await?;
        if current.revision != expected_revision {
            return Err(crate::error::Error::Precondition.into());
        }
        if *tracking != current.tracking {
            validate_admission(tracking)
                .map_err(|error| log_validation_error(&request, error, "update"))?;
        }
    }
    let shipment = request
        .context()
        .store
        .update(id, expected_revision, input, &principal.actor, None)
        .await?;
    slog::info!(request.log, "tracking_registration";
        "request_id" => request.request_id.clone(), "shipment_id" => shipment.id.to_string(),
        "operation" => "update", "outcome" => "saved");
    Ok(shipment_response(shipment))
}

fn validate_admission(tracking: &Tracking) -> Result<(), crate::error::Error> {
    let alphanumeric = tracking
        .number
        .bytes()
        .filter(u8::is_ascii_alphanumeric)
        .count();
    if !(6..=100).contains(&alphanumeric)
        || tracking.number.len() > 100
        || !tracking
            .number
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        || !tracking.number.bytes().any(|byte| byte.is_ascii_digit())
    {
        return Err(crate::error::Error::TrackingInvalid);
    }
    Ok(())
}

fn log_validation_error(
    request: &RequestContext<Arc<Context>>,
    error: crate::error::Error,
    operation: &'static str,
) -> HttpError {
    let error: HttpError = error.into();
    slog::warn!(request.log, "tracking_verification";
        "request_id" => request.request_id.clone(), "operation" => operation,
        "outcome" => "rejected", "error_code" => error.error_code.clone());
    error
}

#[endpoint {method=POST,path="/v1/shipments/{id}/sources",tags=["shipments"]}]
async fn shipment_source(
    request: RequestContext<Arc<Context>>,
    path: Path<ShipmentPath>,
    body: TypedBody<Source>,
) -> Result<ShipmentResponse, HttpError> {
    let principal = authenticate(&request, false).await?;
    Ok(shipment_response(
        request
            .context()
            .store
            .add_source(
                path.into_inner().id,
                revision(&request)?,
                body.into_inner(),
                &principal.actor,
            )
            .await?,
    ))
}

#[endpoint {method=PUT,path="/v1/shipments/{id}/status-override",tags=["shipments"]}]
async fn override_set(
    request: RequestContext<Arc<Context>>,
    path: Path<ShipmentPath>,
    body: TypedBody<SetOverride>,
) -> Result<ShipmentResponse, HttpError> {
    let principal = authenticate(&request, true).await?;
    Ok(shipment_response(
        request
            .context()
            .store
            .set_override(
                path.into_inner().id,
                revision(&request)?,
                Some(body.into_inner()),
                &principal.actor,
            )
            .await?,
    ))
}

#[endpoint {method=DELETE,path="/v1/shipments/{id}/status-override",tags=["shipments"]}]
async fn override_clear(
    request: RequestContext<Arc<Context>>,
    path: Path<ShipmentPath>,
) -> Result<ShipmentResponse, HttpError> {
    let principal = authenticate(&request, true).await?;
    Ok(shipment_response(
        request
            .context()
            .store
            .set_override(
                path.into_inner().id,
                revision(&request)?,
                None,
                &principal.actor,
            )
            .await?,
    ))
}

#[endpoint {method=PUT,path="/v1/shipments/{id}/archive",tags=["shipments"]}]
async fn shipment_archive(
    request: RequestContext<Arc<Context>>,
    path: Path<ShipmentPath>,
) -> Result<ShipmentResponse, HttpError> {
    let principal = authenticate(&request, true).await?;
    Ok(shipment_response(
        request
            .context()
            .store
            .archive(
                path.into_inner().id,
                revision(&request)?,
                true,
                &principal.actor,
            )
            .await?,
    ))
}

#[endpoint {method=DELETE,path="/v1/shipments/{id}/archive",tags=["shipments"]}]
async fn shipment_restore(
    request: RequestContext<Arc<Context>>,
    path: Path<ShipmentPath>,
) -> Result<ShipmentResponse, HttpError> {
    let principal = authenticate(&request, true).await?;
    Ok(shipment_response(
        request
            .context()
            .store
            .archive(
                path.into_inner().id,
                revision(&request)?,
                false,
                &principal.actor,
            )
            .await?,
    ))
}

#[endpoint {method=POST,path="/v1/shipments/{id}/refresh",tags=["shipments"]}]
async fn shipment_refresh(
    request: RequestContext<Arc<Context>>,
    path: Path<ShipmentPath>,
) -> Result<ShipmentResponse, HttpError> {
    let principal = authenticate(&request, false).await?;
    Ok(shipment_response(
        request
            .context()
            .store
            .refresh(path.into_inner().id, revision(&request)?, &principal.actor)
            .await?,
    ))
}

#[derive(Clone, Default, Deserialize, Serialize, JsonSchema)]
struct EventFilters {}
#[derive(Deserialize, Serialize, JsonSchema)]
struct EventCursor {
    shipment_id: Uuid,
    after: i64,
}

#[endpoint {method=GET,path="/v1/shipments/{id}/events",tags=["shipments"]}]
async fn shipment_events(
    request: RequestContext<Arc<Context>>,
    path: Path<ShipmentPath>,
    query: Query<PaginationParams<EventFilters, EventCursor>>,
) -> Result<HttpResponseOk<ResultsPage<Event>>, HttpError> {
    authenticate(&request, false).await?;
    let params = query.into_inner();
    let limit = request.page_limit(&params)?.get().min(200);
    let id = path.into_inner().id;
    let after = match params.page {
        WhichPage::First(_) => 0,
        WhichPage::Next(cursor) => {
            if cursor.shipment_id != id {
                return Err(client_error(
                    StatusCode::BAD_REQUEST,
                    "event cursor belongs to another shipment",
                ));
            }
            cursor.after
        }
    };
    let mut events = request.context().store.events(id, after, limit + 1).await?;
    let more = events.len() > limit as usize;
    events.truncate(limit as usize);
    let mut page = ResultsPage::new(events, &id, |event, id| EventCursor {
        shipment_id: *id,
        after: event.id,
    })?;
    if !more {
        page.next_page = None;
    }
    Ok(HttpResponseOk(page))
}

#[endpoint {method=GET,path="/v1/summary",tags=["system"]}]
async fn summary(
    request: RequestContext<Arc<Context>>,
    query: Query<SummaryFilter>,
) -> Result<HttpResponseOk<Summary>, HttpError> {
    authenticate(&request, false).await?;
    Ok(HttpResponseOk(
        request
            .context()
            .store
            .summary(query.into_inner().direction)
            .await?,
    ))
}

#[derive(Deserialize, JsonSchema)]
struct SummaryFilter {
    direction: Option<Direction>,
}

#[endpoint {method=GET,path="/v1/system",tags=["system"]}]
async fn system(
    request: RequestContext<Arc<Context>>,
) -> Result<HttpResponseOk<System>, HttpError> {
    let principal = authenticate(&request, false).await?;
    let mut system = request
        .context()
        .store
        .system(request.context().provider.is_some())
        .await?;
    system.credential_role = if principal.owner {
        "owner"
    } else if principal.integration {
        "integration"
    } else {
        "agent"
    }
    .into();
    Ok(HttpResponseOk(system))
}

/// Read local installation checks without contacting EasyPost.
#[endpoint {method=GET,path="/v1/installation",tags=["system"]}]
async fn installation(
    request: RequestContext<Arc<Context>>,
) -> Result<HttpResponseOk<InstallationReport>, HttpError> {
    require_owner(&request).await?;
    installation_report(&request).await.map(HttpResponseOk)
}

/// Explicitly check EasyPost using a read-only request, cached for one minute.
#[endpoint {method=POST,path="/v1/installation/check",tags=["system"]}]
async fn installation_check(
    request: RequestContext<Arc<Context>>,
) -> Result<HttpResponseOk<InstallationReport>, HttpError> {
    require_owner(&request).await?;
    let context = request.context();
    let provider = context
        .provider
        .as_ref()
        .map(|provider| provider.with_request_id(&request.request_id));
    context.installation.check_provider(provider.as_ref()).await;
    installation_report(&request).await.map(HttpResponseOk)
}

/// Report a successful integration poll and its Supervisor backup inventory.
#[endpoint {method=POST,path="/v1/integration/presence",tags=["system"]}]
async fn integration_presence(
    request: RequestContext<Arc<Context>>,
    body: TypedBody<IntegrationPresence>,
) -> Result<HttpResponseUpdatedNoContent, HttpError> {
    let principal = authenticate(&request, false).await?;
    if !principal.integration {
        return Err(client_error(
            StatusCode::FORBIDDEN,
            "integration credential required",
        ));
    }
    let presence = body.into_inner();
    if !valid_presence(&presence) {
        return Err(client_error(
            StatusCode::BAD_REQUEST,
            "invalid integration presence",
        ));
    }
    request.context().installation.presence(presence).await;
    Ok(HttpResponseUpdatedNoContent())
}

async fn require_owner(request: &RequestContext<Arc<Context>>) -> Result<(), HttpError> {
    if !authenticate(request, false).await?.owner {
        return Err(client_error(
            StatusCode::FORBIDDEN,
            "owner credential required",
        ));
    }
    Ok(())
}

/// List active agents without exposing their credentials or credential hashes.
#[endpoint {method=GET,path="/v1/agents",tags=["agents"]}]
async fn agents_list(
    request: RequestContext<Arc<Context>>,
) -> Result<HttpResponseHeaders<HttpResponseOk<AgentCredentials>, PrivateHeaders>, HttpError> {
    require_owner(&request).await?;
    Ok(HttpResponseHeaders::new(
        HttpResponseOk(request.context().store.agents().await?),
        private_headers(),
    ))
}

/// Generate a separate ingestion credential. The token is returned only once.
#[endpoint {method=POST,path="/v1/agents",tags=["agents"]}]
async fn agents_create(
    request: RequestContext<Arc<Context>>,
    body: TypedBody<CreateAgent>,
) -> Result<
    HttpResponseHeaders<HttpResponseCreated<CreatedAgentCredential>, PrivateHeaders>,
    HttpError,
> {
    require_owner(&request).await?;
    let token = format!(
        "shp_{}",
        crate::config::random_token()
            .map_err(|_| { HttpError::for_internal_error("generate agent credential".into()) })?
    );
    let digest: [u8; 32] = Sha256::digest(&token).into();
    let credential = request
        .context()
        .store
        .create_agent(&body.into_inner().name, &digest)
        .await?;
    Ok(HttpResponseHeaders::new(
        HttpResponseCreated(CreatedAgentCredential { credential, token }),
        private_headers(),
    ))
}

/// Revoke one agent. Subsequent authentications consult the database directly.
#[endpoint {method=DELETE,path="/v1/agents/{id}",tags=["agents"]}]
async fn agent_revoke(
    request: RequestContext<Arc<Context>>,
    path: Path<ShipmentPath>,
) -> Result<HttpResponseUpdatedNoContent, HttpError> {
    require_owner(&request).await?;
    if !request
        .context()
        .store
        .revoke_agent(path.into_inner().id)
        .await?
    {
        return Err(client_error(StatusCode::NOT_FOUND, "agent not found"));
    }
    Ok(HttpResponseUpdatedNoContent())
}

async fn installation_report(
    request: &RequestContext<Arc<Context>>,
) -> Result<InstallationReport, HttpError> {
    // Test the actual database connection, rather than inferring it from uptime.
    sqlx::query_scalar::<_, i64>("SELECT 1")
        .fetch_one(request.context().store.pool())
        .await
        .map_err(crate::error::Error::from)?;
    Ok(request
        .context()
        .installation
        .report(request.context().provider.is_some())
        .await)
}

#[endpoint {method=GET,path="/openapi.json",tags=["system"]}]
async fn schema(
    request: RequestContext<Arc<Context>>,
) -> Result<HttpResponseOk<serde_json::Value>, HttpError> {
    ingress_access(&request)?;
    Ok(HttpResponseOk(openapi().map_err(|error| {
        HttpError::for_internal_error(error.to_string())
    })?))
}

fn static_response(
    content: &'static str,
    content_type: &'static str,
) -> Result<Response<Body>, HttpError> {
    Response::builder().header("content-type",content_type).header("cache-control","no-store")
        .header("content-security-policy","default-src 'self'; script-src 'self'; style-src 'self'; object-src 'none'; base-uri 'self'; form-action 'self'")
        .header("x-content-type-options","nosniff").body(Body::from(content)).map_err(|error|HttpError::for_internal_error(error.to_string()))
}

#[endpoint {method=GET,path="/",tags=["web"]}]
async fn index(request: RequestContext<Arc<Context>>) -> Result<Response<Body>, HttpError> {
    ingress_access(&request)?;
    static_response(
        include_str!("../web/index.html"),
        "text/html; charset=utf-8",
    )
}
#[endpoint {method=GET,path="/app.js",tags=["web"]}]
async fn script(request: RequestContext<Arc<Context>>) -> Result<Response<Body>, HttpError> {
    ingress_access(&request)?;
    static_response(
        include_str!("../web/app.js"),
        "text/javascript; charset=utf-8",
    )
}
#[endpoint {method=GET,path="/style.css",tags=["web"]}]
async fn style(request: RequestContext<Arc<Context>>) -> Result<Response<Body>, HttpError> {
    ingress_access(&request)?;
    static_response(include_str!("../web/style.css"), "text/css; charset=utf-8")
}

pub fn description() -> Result<ApiDescription<Arc<Context>>> {
    let mut api = ApiDescription::new();
    let endpoints: [dropshot::ApiEndpoint<Arc<Context>>; 24] = [
        shipments_list.into(),
        carriers_list.into(),
        shipments_create.into(),
        shipment_get.into(),
        shipment_update.into(),
        shipment_source.into(),
        override_set.into(),
        override_clear.into(),
        shipment_archive.into(),
        shipment_restore.into(),
        shipment_refresh.into(),
        shipment_events.into(),
        summary.into(),
        system.into(),
        installation.into(),
        installation_check.into(),
        integration_presence.into(),
        agents_list.into(),
        agents_create.into(),
        agent_revoke.into(),
        schema.into(),
        index.into(),
        script.into(),
        style.into(),
    ];
    for endpoint in endpoints {
        api.register(endpoint).context("register API endpoint")?;
    }
    Ok(api)
}

pub fn openapi() -> Result<serde_json::Value> {
    description()?.openapi("Shipments API",semver::Version::parse(env!("CARGO_PKG_VERSION"))?)
        .description("Persistent local shipment records, manual corrections, and carrier synchronization. Use a Bearer owner or ingestion token. Creates require Idempotency-Key; writes to an existing shipment require its quoted revision in If-Match.")
        .json().map_err(Into::into)
}

pub fn start(bind: SocketAddr, context: Arc<Context>) -> Result<HttpServer<Arc<Context>>> {
    start_with_logger(bind, context, crate::logging::logger())
}

pub fn start_with_logger(
    bind: SocketAddr,
    context: Arc<Context>,
    logger: slog::Logger,
) -> Result<HttpServer<Arc<Context>>> {
    HttpServerStarter::new(
        &ConfigDropshot {
            bind_address: bind,
            default_request_body_max_bytes: 64 * 1024,
            default_handler_task_mode: dropshot::HandlerTaskMode::Detached,
            ..Default::default()
        },
        description()?,
        context,
        &logger,
    )
    .map_err(|error| anyhow!("start server: {error}"))
    .map(HttpServerStarter::start)
}
