use crate::{models::*, store::Store};
use anyhow::{Context as _, Result, anyhow};
use dropshot::ClientErrorStatusCode as StatusCode;
use dropshot::{
    ApiDescription, Body, ConfigDropshot, ConfigLogging, ConfigLoggingLevel, HttpError,
    HttpResponseCreated, HttpResponseHeaders, HttpResponseOk, HttpServer, HttpServerStarter,
    PaginationParams, Path, Query, RequestContext, ResultsPage, TypedBody, WhichPage, endpoint,
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

pub struct Context {
    pub store: Store,
    owner_digest: [u8; 32],
    agent_digest: Option<[u8; 32]>,
    ingress_peer: Option<IpAddr>,
    provider_enabled: bool,
}

impl Context {
    pub fn new(
        store: Store,
        owner: &str,
        agent: Option<&str>,
        ingress_peer: Option<IpAddr>,
        provider_enabled: bool,
    ) -> Self {
        Self {
            store,
            owner_digest: Sha256::digest(owner).into(),
            agent_digest: agent.map(|value| Sha256::digest(value).into()),
            ingress_peer,
            provider_enabled,
        }
    }
}

struct Principal {
    actor: String,
    owner: bool,
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

fn authenticate(
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
            }
        } else if context
            .agent_digest
            .as_ref()
            .is_some_and(|expected| bool::from(digest.ct_eq(expected)))
        {
            Principal {
                actor: "agent".into(),
                owner: false,
            }
        } else {
            return Err(client_error(StatusCode::UNAUTHORIZED, "invalid credential"));
        }
    };
    if human_only && !principal.owner {
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
    filters: Filters,
}

/// List local shipment records using stable keyset pagination.
#[endpoint {method=GET,path="/v1/shipments",tags=["shipments"]}]
async fn shipments_list(
    request: RequestContext<Arc<Context>>,
    query: Query<PaginationParams<Filters, ShipmentCursor>>,
) -> Result<HttpResponseOk<ResultsPage<Shipment>>, HttpError> {
    authenticate(&request, false)?;
    let params = query.into_inner();
    let limit = request.page_limit(&params)?.get().min(200);
    let (filters, after) = match &params.page {
        WhichPage::First(filters) => (filters.clone(), None),
        WhichPage::Next(cursor) => (cursor.filters.clone(), Some(cursor.after)),
    };
    let mut items = request
        .context()
        .store
        .list(&filters, after, limit + 1)
        .await?;
    let more = items.len() > limit as usize;
    items.truncate(limit as usize);
    let mut page = ResultsPage::new(items, &filters, |item, filters| ShipmentCursor {
        after: item.id,
        filters: filters.clone(),
    })?;
    if !more {
        page.next_page = None;
    }
    Ok(HttpResponseOk(page))
}

/// Create a durable local record; provider registration happens asynchronously.
#[endpoint {method=POST,path="/v1/shipments",tags=["shipments"]}]
async fn shipments_create(
    request: RequestContext<Arc<Context>>,
    body: TypedBody<CreateShipment>,
) -> Result<HttpResponseHeaders<HttpResponseCreated<Shipment>, VersionHeaders>, HttpError> {
    let principal = authenticate(&request, false)?;
    let key = header(&request, "idempotency-key")
        .ok_or_else(|| client_error(StatusCode::BAD_REQUEST, "Idempotency-Key is required"))?;
    let shipment = request
        .context()
        .store
        .create(body.into_inner(), &principal.actor, key)
        .await?;
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
    authenticate(&request, false)?;
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
    let principal = authenticate(&request, false)?;
    Ok(shipment_response(
        request
            .context()
            .store
            .update(
                path.into_inner().id,
                revision(&request)?,
                body.into_inner(),
                &principal.actor,
            )
            .await?,
    ))
}

#[endpoint {method=POST,path="/v1/shipments/{id}/sources",tags=["shipments"]}]
async fn shipment_source(
    request: RequestContext<Arc<Context>>,
    path: Path<ShipmentPath>,
    body: TypedBody<Source>,
) -> Result<ShipmentResponse, HttpError> {
    let principal = authenticate(&request, false)?;
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
    let principal = authenticate(&request, true)?;
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
    let principal = authenticate(&request, true)?;
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
    let principal = authenticate(&request, true)?;
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
    let principal = authenticate(&request, true)?;
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
    let principal = authenticate(&request, false)?;
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
    authenticate(&request, false)?;
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
) -> Result<HttpResponseOk<Summary>, HttpError> {
    authenticate(&request, false)?;
    Ok(HttpResponseOk(request.context().store.summary().await?))
}

#[endpoint {method=GET,path="/v1/system",tags=["system"]}]
async fn system(
    request: RequestContext<Arc<Context>>,
) -> Result<HttpResponseOk<System>, HttpError> {
    let principal = authenticate(&request, false)?;
    let mut system = request
        .context()
        .store
        .system(request.context().provider_enabled)
        .await?;
    system.credential_role = if principal.owner { "owner" } else { "agent" }.into();
    Ok(HttpResponseOk(system))
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
    let endpoints: [dropshot::ApiEndpoint<Arc<Context>>; 17] = [
        shipments_list.into(),
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
    let logger = ConfigLogging::StderrTerminal {
        level: ConfigLoggingLevel::Warn,
    }
    .to_logger("shipments")
    .map_err(|error| anyhow!("logger: {error}"))?;
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
