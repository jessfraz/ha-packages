use std::sync::Arc;

use dropshot::{
    ApiDescription, HttpError, HttpResponseAccepted, HttpResponseCreated, HttpResponseHeaders,
    HttpResponseOk, PaginationParams, Path, Query, RequestContext, ResultsPage, TypedBody,
    WhichPage, endpoint,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    api::{Context, pickup_authorize, pickup_prepare_watches},
    pickup::{Error, ErrorCode, models::*, service::CarrierStatus},
};

#[derive(Serialize, JsonSchema)]
struct Headers {
    #[serde(rename = "cache-control")]
    cache_control: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    etag: Option<String>,
}
fn headers(revision: Option<i64>) -> Headers {
    Headers {
        cache_control: "no-store",
        etag: revision.map(|r| format!("\"{r}\"")),
    }
}
type Private<T> = HttpResponseHeaders<HttpResponseOk<T>, Headers>;
fn private<T: Serialize + JsonSchema + Send + Sync + 'static>(
    value: T,
    revision: Option<i64>,
) -> Private<T> {
    HttpResponseHeaders::new(HttpResponseOk(value), headers(revision))
}

#[derive(Deserialize, JsonSchema)]
struct Id {
    id: Uuid,
}
#[derive(Deserialize, JsonSchema)]
struct Label {
    tracking: String,
}
#[derive(Deserialize, JsonSchema)]
struct CarrierQuery {
    carrier: Option<PickupCarrier>,
}
#[derive(Deserialize, JsonSchema)]
struct ResolveQuery {
    reference: String,
    carrier: Option<PickupCarrier>,
}
#[derive(Clone, Default, Deserialize, Serialize, JsonSchema)]
struct Filter {}
#[derive(Deserialize, Serialize, JsonSchema)]
struct Cursor {
    after: Uuid,
}

fn revision(request: &RequestContext<Arc<Context>>) -> Result<i64, HttpError> {
    request
        .request
        .headers()
        .get("if-match")
        .and_then(|h| h.to_str().ok())
        .and_then(|v| v.strip_prefix('"').and_then(|v| v.strip_suffix('"')))
        .and_then(|v| v.parse().ok())
        .filter(|r| *r > 0)
        .ok_or_else(|| {
            HttpError::for_client_error(
                Some("pickup_revision_required".into()),
                dropshot::ClientErrorStatusCode::PRECONDITION_REQUIRED,
                "Use the current pickup or location ETag in If-Match".into(),
            )
        })
}
fn key(request: &RequestContext<Arc<Context>>) -> Result<&str, HttpError> {
    request
        .request
        .headers()
        .get("idempotency-key")
        .and_then(|h| h.to_str().ok())
        .filter(|v| !v.is_empty() && v.len() <= 200 && v.bytes().all(|c| c.is_ascii_graphic()))
        .ok_or_else(|| Error::new(ErrorCode::InvalidInput).into())
}

/// Minimal read-only Home Assistant counts and calendar windows, without private label/site data.
#[endpoint{method=GET,path="/v1/pickups/status",tags=["pickups"]}]
async fn status(request: RequestContext<Arc<Context>>) -> Result<Private<PickupStatus>, HttpError> {
    crate::api::pickup_status_authorize(&request).await?;
    Ok(private(request.context().pickups.status().await?, None))
}

/// Direct pickup carriers and their actual account/capability status.
#[endpoint{method=GET,path="/v1/pickups/carriers",tags=["pickups"]}]
async fn carriers(
    request: RequestContext<Arc<Context>>,
) -> Result<Private<Vec<CarrierStatus>>, HttpError> {
    pickup_authorize(&request).await?;
    Ok(private(request.context().pickups.carriers(), None))
}

/// Saved pickup sites for this deployment. Billing addresses are not inferred.
#[endpoint{method=GET,path="/v1/pickups/locations",tags=["pickups"]}]
async fn locations(request: RequestContext<Arc<Context>>) -> Result<Private<Vec<Site>>, HttpError> {
    pickup_authorize(&request).await?;
    Ok(private(request.context().pickups.sites().await?, None))
}

/// Save a confirmed site and optionally make it the deployment default.
#[endpoint{method=POST,path="/v1/pickups/locations",tags=["pickups"]}]
async fn location_create(
    request: RequestContext<Arc<Context>>,
    body: TypedBody<SaveSite>,
) -> Result<HttpResponseHeaders<HttpResponseCreated<Site>, Headers>, HttpError> {
    pickup_authorize(&request).await?;
    let body = body.into_inner();
    let site = request
        .context()
        .pickups
        .save_site(None, None, body.location, body.make_default)
        .await?;
    let revision = site.revision;
    Ok(HttpResponseHeaders::new(
        HttpResponseCreated(site),
        headers(Some(revision)),
    ))
}

/// Replace a saved site without silently changing already scheduled pickups.
#[endpoint{method=PUT,path="/v1/pickups/locations/{id}",tags=["pickups"]}]
async fn location_update(
    request: RequestContext<Arc<Context>>,
    path: Path<Id>,
    body: TypedBody<SaveSite>,
) -> Result<Private<Site>, HttpError> {
    pickup_authorize(&request).await?;
    let revision = revision(&request)?;
    let body = body.into_inner();
    let site = request
        .context()
        .pickups
        .save_site(
            Some(path.into_inner().id),
            Some(revision),
            body.location,
            body.make_default,
        )
        .await?;
    let revision = site.revision;
    Ok(private(site, Some(revision)))
}

/// Read a label's explicitly saved service, weight and destination.
#[endpoint{method=GET,path="/v1/pickups/parcels/{tracking}",tags=["pickups"]}]
async fn parcel_get(
    request: RequestContext<Arc<Context>>,
    path: Path<Label>,
    query: Query<CarrierQuery>,
) -> Result<Private<Option<Parcel>>, HttpError> {
    pickup_authorize(&request).await?;
    Ok(private(
        request
            .context()
            .pickups
            .parcel(&path.into_inner().tracking, query.into_inner().carrier)
            .await?,
        None,
    ))
}

/// Save known label details; service and weight are never guessed from a barcode.
#[endpoint{method=PUT,path="/v1/pickups/parcels/{tracking}",tags=["pickups"]}]
async fn parcel_save(
    request: RequestContext<Arc<Context>>,
    path: Path<Label>,
    body: TypedBody<ParcelInput>,
) -> Result<Private<Parcel>, HttpError> {
    pickup_authorize(&request).await?;
    Ok(private(
        request
            .context()
            .pickups
            .save_parcel(&path.into_inner().tracking, body.into_inner())
            .await?,
        None,
    ))
}

/// Read-only pickup availability at the selected/default saved location.
#[endpoint{method=POST,path="/v1/pickups/availability",tags=["pickups"]}]
async fn availability(
    request: RequestContext<Arc<Context>>,
    body: TypedBody<ScheduleRequest>,
) -> Result<Private<Offer>, HttpError> {
    pickup_authorize(&request).await?;
    let result = request
        .context()
        .pickups
        .availability(&body.into_inner())
        .await?;
    pickup_authorize(&request).await?;
    Ok(private(result, None))
}

/// Optional read-only eligibility and fee inspection. No driver is dispatched.
#[endpoint{method=POST,path="/v1/pickups/preview",tags=["pickups"]}]
async fn preview(
    request: RequestContext<Arc<Context>>,
    body: TypedBody<ScheduleRequest>,
) -> Result<Private<Vec<Preview>>, HttpError> {
    pickup_authorize(&request).await?;
    let result = request
        .context()
        .pickups
        .preview(&body.into_inner())
        .await?;
    pickup_authorize(&request).await?;
    Ok(private(result, None))
}

/// Verify/reuse tracking watchers, then enqueue independently fenced pickups.
#[endpoint{method=POST,path="/v1/pickups",tags=["pickups"]}]
async fn schedule(
    request: RequestContext<Arc<Context>>,
    body: TypedBody<ScheduleRequest>,
) -> Result<HttpResponseHeaders<HttpResponseAccepted<Batch>, Headers>, HttpError> {
    let actor = pickup_authorize(&request).await?;
    let operation_key = key(&request)?.to_owned();
    let body = body.into_inner();
    if let Some(receipt) = request
        .context()
        .pickups
        .replay(&actor, &operation_key, &body)
        .await?
    {
        pickup_authorize(&request).await?;
        return Ok(HttpResponseHeaders::new(
            HttpResponseAccepted(receipt),
            headers(None),
        ));
    }
    request.context().pickups.check_reviewed_site(&body).await?;
    pickup_prepare_watches(&request, &body).await?;
    let actor = pickup_authorize(&request).await?;
    let receipt = request
        .context()
        .pickups
        .schedule(&actor, &operation_key, &body)
        .await?;
    Ok(HttpResponseHeaders::new(
        HttpResponseAccepted(receipt),
        headers(None),
    ))
}

/// Poll one durable multi-carrier scheduling request, including partial results.
#[endpoint{method=GET,path="/v1/pickups/batches/{id}",tags=["pickups"]}]
async fn batch(
    request: RequestContext<Arc<Context>>,
    path: Path<Id>,
) -> Result<Private<Batch>, HttpError> {
    pickup_authorize(&request).await?;
    Ok(private(
        request
            .context()
            .pickups
            .batch(path.into_inner().id)
            .await?,
        None,
    ))
}

/// List one bounded page of pickup records, newest first.
#[endpoint{method=GET,path="/v1/pickups",tags=["pickups"]}]
async fn list(
    request: RequestContext<Arc<Context>>,
    query: Query<PaginationParams<Filter, Cursor>>,
) -> Result<Private<ResultsPage<Record>>, HttpError> {
    pickup_authorize(&request).await?;
    let params = query.into_inner();
    let limit = request.page_limit(&params)?.get().min(200);
    let after = match params.page {
        WhichPage::First(_) => None,
        WhichPage::Next(cursor) => Some(cursor.after),
    };
    let mut rows = request.context().pickups.list(after, limit + 1).await?;
    let more = rows.len() > usize::try_from(limit).unwrap_or(200);
    rows.truncate(usize::try_from(limit).unwrap_or(200));
    let mut page = ResultsPage::new(rows, &Filter {}, |record, _| Cursor { after: record.id })?;
    if !more {
        page.next_page = None;
    }
    Ok(private(page, None))
}

/// Resolve a tracking number or an advanced pickup ID without choosing an ambiguous match.
#[endpoint{method=GET,path="/v1/pickups/resolve",tags=["pickups"]}]
async fn resolve(
    request: RequestContext<Arc<Context>>,
    query: Query<ResolveQuery>,
) -> Result<Private<Record>, HttpError> {
    pickup_authorize(&request).await?;
    let query = query.into_inner();
    let record = request
        .context()
        .pickups
        .resolve(&query.reference, query.carrier)
        .await?;
    let revision = record.revision;
    Ok(private(record, Some(revision)))
}

/// Read a pickup, its recorded charge information and carrier confirmation.
#[endpoint{method=GET,path="/v1/pickups/records/{id}",tags=["pickups"]}]
async fn get(
    request: RequestContext<Arc<Context>>,
    path: Path<Id>,
) -> Result<Private<Record>, HttpError> {
    pickup_authorize(&request).await?;
    let record = request.context().pickups.get(path.into_inner().id).await?;
    let revision = record.revision;
    Ok(private(record, Some(revision)))
}

/// Refresh only through a documented carrier lookup. FedEx has no such API.
#[endpoint{method=POST,path="/v1/pickups/records/{id}/refresh",tags=["pickups"]}]
async fn refresh(
    request: RequestContext<Arc<Context>>,
    path: Path<Id>,
) -> Result<Private<Record>, HttpError> {
    pickup_authorize(&request).await?;
    let record = request
        .context()
        .pickups
        .refresh(path.into_inner().id, revision(&request)?)
        .await?;
    pickup_authorize(&request).await?;
    let revision = record.revision;
    Ok(private(record, Some(revision)))
}

/// Enqueue one explicit carrier cancellation. A cancellation is not a refund.
#[endpoint{method=POST,path="/v1/pickups/records/{id}/cancel",tags=["pickups"]}]
async fn cancel(
    request: RequestContext<Arc<Context>>,
    path: Path<Id>,
) -> Result<HttpResponseHeaders<HttpResponseAccepted<Record>, Headers>, HttpError> {
    let actor = pickup_authorize(&request).await?;
    let record = request
        .context()
        .pickups
        .cancel(
            path.into_inner().id,
            revision(&request)?,
            &actor,
            key(&request)?,
        )
        .await?;
    let revision = record.revision;
    Ok(HttpResponseHeaders::new(
        HttpResponseAccepted(record),
        headers(Some(revision)),
    ))
}

/// Resolve an unknown outcome only after an explicit operator carrier check.
#[endpoint{method=POST,path="/v1/pickups/records/{id}/reconcile",tags=["pickups"]}]
async fn reconcile(
    request: RequestContext<Arc<Context>>,
    path: Path<Id>,
    body: TypedBody<ReconcileRequest>,
) -> Result<Private<Record>, HttpError> {
    let actor = pickup_authorize(&request).await?;
    let record = request
        .context()
        .pickups
        .reconcile(
            path.into_inner().id,
            revision(&request)?,
            &actor,
            body.into_inner(),
        )
        .await?;
    pickup_authorize(&request).await?;
    let revision = record.revision;
    Ok(private(record, Some(revision)))
}

pub(crate) fn register(api: &mut ApiDescription<Arc<Context>>) -> anyhow::Result<()> {
    let endpoints: [dropshot::ApiEndpoint<Arc<Context>>; 17] = [
        carriers.into(),
        status.into(),
        reconcile.into(),
        locations.into(),
        location_create.into(),
        location_update.into(),
        parcel_get.into(),
        parcel_save.into(),
        availability.into(),
        preview.into(),
        schedule.into(),
        batch.into(),
        list.into(),
        resolve.into(),
        get.into(),
        refresh.into(),
        cancel.into(),
    ];
    for endpoint in endpoints {
        api.register(endpoint)?;
    }
    Ok(())
}
