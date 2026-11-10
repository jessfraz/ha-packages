//! A transport-only client. The server owns validation, authorization and tracking.
use clap::{Args, Parser, Subcommand, ValueEnum};
use packages::{
    carrier::Carrier,
    installation::InstallationReport,
    models::*,
    pickup::{
        models::{
            Batch as PickupBatch, Offer as PickupOffer, Parcel as PickupParcel, ParcelInput,
            PickupCarrier, PickupService, Preview as PickupPreview, Record as PickupRecord,
            ScheduleRequest, Site as PickupSite, State as PickupState,
        },
        service::CarrierStatus,
    },
};
use reqwest::{Client, Method, Url, header};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{
    env,
    fs::File,
    io::{self, Read, Write},
    process::ExitCode,
    time::Duration,
};
use uuid::Uuid;

const MAX_INPUT: u64 = 1024 * 1024;
const MAX_RESPONSE: usize = 8 * 1024 * 1024;

type Result<T> = std::result::Result<T, Failure>;

#[derive(Parser)]
#[command(
    name = "ha-packages",
    version,
    about = "JSON client for the Packages API",
    long_about = "JSON client for the Packages API. Configure PACKAGES_URL and PACKAGES_API_TOKEN (or PACKAGES_AGENT_TOKEN), or HOME_ASSISTANT_URL and HOME_ASSISTANT_TOKEN. Never retries or follows redirects. Each paginated command makes one request."
)]
struct Cli {
    /// Required when both native and Home Assistant URLs are configured.
    #[arg(long, global = true, value_enum)]
    transport: Option<Transport>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Clone, Copy, ValueEnum)]
enum Transport {
    Native,
    HomeAssistant,
}

#[derive(Args)]
struct Input {
    /// JSON file, or - for stdin. Never interpreted as a URL.
    #[arg(long, default_value = "-")]
    input: String,
}

#[derive(Args)]
struct Page {
    #[arg(long, value_parser = clap::value_parser!(u32).range(1..=200))]
    limit: Option<u32>,
    #[arg(long)]
    page_token: Option<String>,
}

#[derive(Args)]
struct Existing {
    /// Tracking number, or an advanced package UUID.
    id: String,
    /// Optional carrier filter; never changes a saved tracking identity.
    #[arg(long)]
    carrier: Option<String>,
    /// Override the automatically fetched current ETag for conditional automation.
    #[arg(long, allow_hyphen_values = true)]
    if_match: Option<String>,
}

#[derive(Clone, Copy, ValueEnum)]
enum CliDirection {
    Inbound,
    Outbound,
}

#[derive(Args)]
struct CreateArgs {
    /// Label tracking number. Omit when using --input or JSON on stdin.
    tracking: Option<String>,
    #[arg(long, conflicts_with = "input")]
    carrier: Option<String>,
    #[arg(long, value_enum, conflicts_with = "input")]
    direction: Option<CliDirection>,
    #[arg(long, conflicts_with = "input")]
    title: Option<String>,
    /// Advanced full CreateShipment JSON file, or - for stdin.
    #[arg(long, conflicts_with = "tracking")]
    input: Option<String>,
    /// Optional stable key for automation; a key is generated for normal use.
    #[arg(long)]
    idempotency_key: Option<String>,
}

#[derive(Args)]
struct PickupPlanArgs {
    /// Prepaid label tracking numbers. Existing verified watchers are reused.
    tracking: Vec<String>,
    /// Explicit carrier; otherwise use saved/verified tracking identity.
    #[arg(long, value_enum)]
    carrier: Option<PickupCarrier>,
    /// Saved pickup site; otherwise this deployment's confirmed default.
    #[arg(long)]
    site: Option<Uuid>,
    /// Local date YYYY-MM-DD; otherwise request the next eligible date.
    #[arg(long)]
    date: Option<String>,
    #[arg(long)]
    ready_time: Option<String>,
    #[arg(long)]
    close_time: Option<String>,
    /// Actual label service, needed when it has not already been saved.
    #[arg(long, value_enum)]
    service: Option<PickupService>,
    /// Actual label weight in pounds, to two decimal places (single label).
    #[arg(long)]
    weight_lb: Option<String>,
    /// Piece count for a standalone pickup without tracking labels.
    #[arg(long)]
    pieces: Option<u16>,
    #[arg(long)]
    destination_country: Option<String>,
}

impl PickupPlanArgs {
    fn request(self, accept_fee: bool) -> ScheduleRequest {
        ScheduleRequest {
            tracking: self.tracking,
            carrier: self.carrier,
            site_id: self.site,
            reviewed_site: None,
            date: self.date,
            ready_time: self.ready_time,
            close_time: self.close_time,
            service: self.service,
            weight_lbs: self.weight_lb,
            pieces: self.pieces,
            destination_country: self.destination_country,
            accept_fee,
        }
    }
}

#[derive(Subcommand)]
enum PickupCommand {
    /// List USPS, UPS and FedEx account configuration and pickup capabilities.
    Carriers,
    /// Manage this deployment's confirmed pickup addresses and contact details.
    Locations {
        #[command(subcommand)]
        command: LocationCommand,
    },
    /// Save known service and weight details for an existing prepaid label.
    Parcel {
        #[command(subcommand)]
        command: ParcelCommand,
    },
    /// Check read-only carrier availability at the saved/default pickup site.
    Availability {
        #[arg(long, value_enum)]
        carrier: PickupCarrier,
        #[arg(long)]
        site: Option<Uuid>,
        #[arg(long)]
        date: Option<String>,
        #[arg(long)]
        ready_time: Option<String>,
        #[arg(long)]
        close_time: Option<String>,
        #[arg(long, value_enum)]
        service: Option<PickupService>,
        #[arg(long)]
        destination_country: Option<String>,
    },
    /// Inspect eligibility, pickup details and available fee information only.
    #[command(visible_alias = "quote")]
    Preview {
        #[command(flatten)]
        plan: PickupPlanArgs,
    },
    /// Add/verify missing watchers, then schedule all selected labels in one command.
    Schedule {
        #[command(flatten)]
        plan: PickupPlanArgs,
        /// Accept whatever pickup fee the configured carrier account is charged.
        #[arg(long)]
        accept_fee: bool,
        #[arg(long)]
        idempotency_key: Option<String>,
    },
    /// List one page of saved pickup confirmations and uncertain requests.
    List {
        #[command(flatten)]
        page: Page,
    },
    /// Read a pickup using its tracking number or an advanced pickup UUID.
    Get {
        reference: String,
        #[arg(long, value_enum)]
        carrier: Option<PickupCarrier>,
    },
    /// Retrieve carrier-confirmed details where a documented lookup exists.
    Refresh {
        reference: String,
        #[arg(long, value_enum)]
        carrier: Option<PickupCarrier>,
        #[arg(long)]
        if_match: Option<String>,
    },
    /// Resolve an unknown outcome after checking the carrier portal/support.
    Reconcile {
        reference: String,
        #[arg(long, value_enum)]
        carrier: Option<PickupCarrier>,
        #[arg(long)]
        if_match: Option<String>,
        #[command(flatten)]
        input: Input,
    },
    /// Cancel a pickup explicitly and wait for the carrier's recorded outcome.
    Cancel {
        reference: String,
        #[arg(long, value_enum)]
        carrier: Option<PickupCarrier>,
        #[arg(long)]
        if_match: Option<String>,
        #[arg(long)]
        idempotency_key: Option<String>,
    },
}

#[derive(Subcommand)]
enum LocationCommand {
    /// List saved locations and the confirmed default for this HA deployment.
    List,
    /// Create a confirmed default location from private JSON on stdin or a file.
    Create {
        #[command(flatten)]
        input: Input,
    },
    /// Replace a saved location; scheduled pickups keep their original address.
    Update {
        id: Uuid,
        #[arg(long)]
        if_match: String,
        #[command(flatten)]
        input: Input,
    },
}

#[derive(Subcommand)]
enum ParcelCommand {
    /// Read saved pickup service/weight information for one tracking label.
    Get {
        tracking: String,
        #[arg(long, value_enum)]
        carrier: Option<PickupCarrier>,
    },
    /// Store actual label details without creating a driver pickup.
    Set {
        tracking: String,
        #[arg(long, value_enum)]
        carrier: PickupCarrier,
        #[arg(long, value_enum)]
        service: PickupService,
        #[arg(long)]
        weight_lb: String,
        #[arg(long, default_value = "US")]
        destination_country: String,
    },
}

#[derive(Subcommand)]
enum Command {
    /// List one page. Optional JSON filters use the API's Filters model.
    List {
        #[arg(long)]
        input: Option<String>,
        #[command(flatten)]
        page: Page,
    },
    /// Read a package by its label tracking number or advanced UUID.
    Get {
        id: String,
        #[arg(long)]
        carrier: Option<String>,
    },
    /// Add and verify a tracking watcher from a label, or advanced JSON input.
    Create {
        #[command(flatten)]
        args: CreateArgs,
    },
    /// Change package metadata, preserving fields not supplied.
    Patch {
        #[command(flatten)]
        existing: Existing,
        #[command(flatten)]
        input: Input,
    },
    /// Attach one source reference to an existing package.
    AddSource {
        #[command(flatten)]
        existing: Existing,
        #[command(flatten)]
        input: Input,
    },
    /// Record an explicit human correction without changing carrier evidence.
    SetOverride {
        #[command(flatten)]
        existing: Existing,
        #[command(flatten)]
        input: Input,
    },
    /// Remove a manual correction and show carrier-reported status.
    ClearOverride {
        #[command(flatten)]
        existing: Existing,
    },
    /// Archive a package without deleting its history.
    Archive {
        #[command(flatten)]
        existing: Existing,
    },
    /// Restore an archived package and resume eligible tracking work.
    Restore {
        #[command(flatten)]
        existing: Existing,
    },
    /// Request a fresh tracking check without creating another watcher.
    Refresh {
        #[command(flatten)]
        existing: Existing,
    },
    /// Read one page of a package's recorded history using tracking or UUID.
    History {
        id: String,
        #[arg(long)]
        carrier: Option<String>,
        #[command(flatten)]
        page: Page,
    },
    /// Read one page of cached, typed delivery-date estimates.
    DeliveryEstimates {
        #[command(flatten)]
        page: Page,
    },
    /// Read one durable state-transition page (native API only).
    Events {
        #[arg(long)]
        after: Option<i64>,
        #[arg(long, value_parser = clap::value_parser!(u32).range(1..=200))]
        limit: Option<u32>,
    },
    /// Read cached package counts and health for the selected filters.
    Summary {
        #[arg(long)]
        input: Option<String>,
    },
    /// List all supported tracking products, not just pickup-capable carriers.
    Carriers,
    /// Read application version, provider and credential-role status.
    System,
    /// Direct USPS, UPS and FedEx pickup setup, scheduling and confirmations.
    Pickups {
        #[command(subcommand)]
        command: PickupCommand,
    },
    /// Owner-only native API operations, not exposed by the HA proxy.
    /// Read native installation, pairing and backup checks.
    Installation,
    /// Run native connection checks; not exposed by the HA proxy.
    CheckInstallation,
    /// List native tracking-ingestion credentials.
    Agents,
    /// Create a native tracking credential; never grants physical pickup authority.
    CreateAgent {
        #[command(flatten)]
        input: Input,
    },
    /// Revoke one native tracking-ingestion credential.
    RevokeAgent { id: Uuid },
}

// No provider error strings, URLs, credentials, response bodies or input values
// cross the stderr boundary, even when a remote endpoint is malicious.
#[derive(Serialize)]
struct Failure {
    #[serde(skip_serializing_if = "Option::is_none")]
    error_code: Option<ApiErrorCode>,
    error: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    hint: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    status: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    retry_after_seconds: Option<u64>,
}
impl Failure {
    fn new(error: &'static str) -> Self {
        Self {
            error,
            hint: None,
            error_code: None,
            status: None,
            retry_after_seconds: None,
        }
    }
}

// The service's normalized codes are a closed diagnostic boundary. Unknown
// codes are omitted rather than allowing arbitrary remote text onto stderr.
#[derive(Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum ApiErrorCode {
    CarrierRequired,
    TrackingRequired,
    TrackingInvalid,
    TrackingRecipientUrlRequired,
    TrackingRecipientUrlInvalid,
    InvalidInput,
    NotFound,
    Conflict,
    TrackingDuplicate,
    RevisionConflict,
    InternalError,
    TrackingVerificationPending,
    RegistrationAmbiguous,
    RegistrationExpired,
    ProviderConfiguration,
    ProviderTimeout,
    LocalProviderQueueTimeout,
    LocalProviderQueueFull,
    ProviderConnection,
    RecipientPageUnavailable,
    ProviderThrottled,
    ProviderResponseTooLarge,
    ProviderInvalidResponse,
    TrackingProviderCredentials,
    TrackingVerificationRateLimited,
    ProviderHttp,
    TrackingUnsupported,
    TrackingCarrierMismatch,
    TrackingCarrierAmbiguous,
    TrackingTestKey,
    TrackingCarrierAccountRequired,
    TrackingCarrierCredentialsRequired,
    TrackingRecipientUrlMismatch,
    PickupInvalidInput,
    PickupNotConfigured,
    PickupUnauthorized,
    PickupUnavailable,
    PickupRateLimited,
    PickupIneligible,
    PickupNotFound,
    PickupConflict,
    PickupUnsupported,
    PickupUnknownOutcome,
    PickupStorageUnavailable,
    PickupFeeConsentRequired,
    PickupParcelDetailsRequired,
    PickupWatchNotReady,
    PickupLocationRequired,
    PickupLocationChanged,
    PickupAmbiguous,
    PickupRevisionRequired,
}

#[derive(Deserialize)]
struct ErrorResponse {
    error_code: Option<ApiErrorCode>,
}

async fn http_failure(mut response: reqwest::Response, error: &'static str) -> Failure {
    let mut failure = Failure {
        error,
        hint: None,
        error_code: None,
        status: Some(response.status().as_u16()),
        retry_after_seconds: response
            .headers()
            .get(header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse().ok()),
    };
    if response.status().is_redirection() {
        return failure;
    }
    let mut bytes = Vec::new();
    while let Ok(Some(chunk)) = response.chunk().await {
        if bytes.len().saturating_add(chunk.len()) > 64 * 1024 {
            return failure;
        }
        bytes.extend_from_slice(&chunk);
    }
    failure.error_code = serde_json::from_slice::<ErrorResponse>(&bytes)
        .ok()
        .and_then(|body| body.error_code);
    failure.hint = match &failure.error_code {
        Some(ApiErrorCode::PickupParcelDetailsRequired) => Some(
            "Save the label's service and weight with pickups parcel set, or pass --service and --weight-lb for a single label.",
        ),
        Some(ApiErrorCode::PickupLocationRequired) => Some(
            "Save this deployment's confirmed default address with pickups locations create --input FILE.",
        ),
        Some(ApiErrorCode::PickupFeeConsentRequired) => Some(
            "Pass boolean --accept-fee to bill the configured carrier shipping account for whatever pickup fee is assessed.",
        ),
        Some(ApiErrorCode::PickupUnknownOutcome) => Some(
            "Do not book again. Reconcile with the carrier first; rerunning the command reuses the uncertain request.",
        ),
        Some(ApiErrorCode::PickupAmbiguous | ApiErrorCode::CarrierRequired) => {
            Some("Pass --carrier usps, ups, or fedex when the tracking number is ambiguous.")
        }
        Some(ApiErrorCode::PickupNotConfigured | ApiErrorCode::PickupUnauthorized) => Some(
            "Configure the direct carrier developer credentials and linked shipping account before scheduling.",
        ),
        _ => None,
    };
    failure
}

#[derive(Serialize)]
struct Output<T> {
    status: u16,
    etag: Option<String>,
    data: T,
}

#[derive(Deserialize, Serialize)]
struct PageResponse<T> {
    items: Vec<T>,
    next_page: Option<String>,
}

#[derive(Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct SummaryQuery {
    #[serde(skip_serializing_if = "Option::is_none")]
    direction: Option<Direction>,
    #[serde(skip_serializing_if = "Option::is_none")]
    delivered_after: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    delivered_before: Option<i64>,
}

struct Api {
    client: Client,
    base: Url,
    token: header::HeaderValue,
    home_assistant: bool,
}

fn variable(name: &str) -> Result<Option<String>> {
    env::var(name).map(Some).or_else(|e| match e {
        env::VarError::NotPresent => Ok(None),
        _ => Err(Failure::new("invalid_environment")),
    })
}

impl Api {
    fn new(transport: Option<Transport>) -> Result<Self> {
        let native = variable("PACKAGES_URL")?;
        let ha = variable("HOME_ASSISTANT_URL")?;
        let home_assistant = match transport {
            Some(Transport::Native) => false,
            Some(Transport::HomeAssistant) => true,
            None => match (&native, &ha) {
                (Some(_), None) => false,
                (None, Some(_)) => true,
                _ => return Err(Failure::new("select_transport_and_configure_url")),
            },
        };
        let raw =
            if home_assistant { ha } else { native }.ok_or_else(|| Failure::new("missing_url"))?;
        let mut base = Url::parse(&raw).map_err(|_| Failure::new("invalid_url"))?;
        // Only an origin is accepted. No ingress URLs, credentials or user-chosen
        // API paths: auth for these two transports is deliberately distinct.
        if !matches!(base.scheme(), "http" | "https")
            || base.host_str().is_none()
            || !base.username().is_empty()
            || base.password().is_some()
            || base.query().is_some()
            || base.fragment().is_some()
            || base.path() != "/"
        {
            return Err(Failure::new("url_must_be_http_origin"));
        }
        base.set_path(if home_assistant {
            "/api/packages/v1/"
        } else {
            "/v1/"
        });
        let token = if home_assistant {
            variable("HOME_ASSISTANT_TOKEN")?
        } else {
            match (
                variable("PACKAGES_API_TOKEN")?,
                variable("PACKAGES_AGENT_TOKEN")?,
            ) {
                (Some(_), Some(_)) => return Err(Failure::new("select_one_native_token")),
                (owner, agent) => owner.or(agent),
            }
        }
        .filter(|v| !v.is_empty())
        .ok_or_else(|| Failure::new("missing_token"))?;
        let mut token = header::HeaderValue::from_str(&format!("Bearer {token}"))
            .map_err(|_| Failure::new("invalid_token"))?;
        token.set_sensitive(true);
        let client = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(120))
            .build()
            .map_err(|_| Failure::new("http_client_configuration"))?;
        Ok(Self {
            client,
            base,
            token,
            home_assistant,
        })
    }

    fn request(&self, method: Method, path: &str) -> Result<reqwest::RequestBuilder> {
        let url = self
            .base
            .join(path)
            .map_err(|_| Failure::new("invalid_route"))?;
        Ok(self
            .client
            .request(method, url)
            .header(header::AUTHORIZATION, self.token.clone())
            .header(header::ACCEPT, "application/json"))
    }

    async fn resolve(&self, reference: &str, carrier: Option<&str>) -> Result<Output<Shipment>> {
        let mut request = self
            .request(Method::GET, "package-reference")?
            .query(&[("reference", reference)]);
        if let Some(carrier) = carrier {
            request = request.query(&[("carrier", carrier)]);
        }
        self.fetch(request).await
    }

    async fn package_id(&self, reference: &str, carrier: Option<&str>) -> Result<Uuid> {
        if carrier.is_none()
            && let Ok(id) = Uuid::parse_str(reference)
        {
            return Ok(id);
        }
        Ok(self.resolve(reference, carrier).await?.data.id)
    }

    async fn existing(
        &self,
        method: Method,
        existing: &Existing,
        suffix: &str,
    ) -> Result<reqwest::RequestBuilder> {
        let id = Uuid::parse_str(&existing.id).ok();
        let current = if id.is_none() || existing.if_match.is_none() || existing.carrier.is_some() {
            Some(
                self.resolve(&existing.id, existing.carrier.as_deref())
                    .await?,
            )
        } else {
            None
        };
        let id = current
            .as_ref()
            .map(|r| r.data.id)
            .or(id)
            .ok_or_else(|| Failure::new("package_not_found"))?;
        // Explicit conditions preserve the caller's revision. Normal commands
        // read once and write conditionally, so racing updates still fail412.
        let value = existing
            .if_match
            .as_deref()
            .or_else(|| current.as_ref().and_then(|r| r.etag.as_deref()))
            .ok_or_else(|| Failure::new("missing_response_etag"))?;
        validate_etag(value)?;
        Ok(self
            .request(method, &format!("packages/{id}{suffix}"))?
            .header(header::IF_MATCH, checked_header(value)?))
    }

    async fn output<T: DeserializeOwned + Serialize>(
        &self,
        request: reqwest::RequestBuilder,
    ) -> Result<()> {
        write_json(&self.fetch::<T>(request).await?)
    }

    async fn fetch<T: DeserializeOwned>(
        &self,
        request: reqwest::RequestBuilder,
    ) -> Result<Output<T>> {
        let mut response = request.send().await.map_err(|e| {
            Failure::new(if e.is_timeout() {
                "request_timeout_outcome_unknown"
            } else {
                "request_failed_outcome_unknown"
            })
        })?;
        let status = response.status();
        if !status.is_success() {
            return Err(http_failure(
                response,
                if status.is_redirection() {
                    "redirect_refused"
                } else {
                    "http_error"
                },
            )
            .await);
        }
        let etag = response
            .headers()
            .get(header::ETAG)
            .map(|v| v.to_str().map(str::to_owned))
            .transpose()
            .map_err(|_| Failure::new("invalid_response_etag"))?;
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| Failure::new("response_read_failed_outcome_unknown"))?
        {
            if bytes.len().saturating_add(chunk.len()) > MAX_RESPONSE {
                return Err(Failure::new("response_too_large_outcome_unknown"));
            }
            bytes.extend_from_slice(&chunk);
        }
        let data: T = serde_json::from_slice(&bytes)
            .map_err(|_| Failure::new("invalid_json_response_outcome_unknown"))?;
        Ok(Output {
            status: status.as_u16(),
            etag,
            data,
        })
    }

    async fn revoke(&self, id: Uuid) -> Result<()> {
        let response = self
            .request(Method::DELETE, &format!("agents/{id}"))?
            .send()
            .await
            .map_err(|_| Failure::new("request_failed_outcome_unknown"))?;
        if response.status() != reqwest::StatusCode::NO_CONTENT {
            return Err(http_failure(response, "unexpected_revoke_status").await);
        }
        write_json(&Output {
            status: 204,
            etag: None,
            data: (),
        })
    }
}

fn checked_header(value: &str) -> Result<header::HeaderValue> {
    if value.is_empty() {
        return Err(Failure::new("empty_header"));
    }
    header::HeaderValue::from_str(value).map_err(|_| Failure::new("invalid_header"))
}

fn validate_etag(value: &str) -> Result<()> {
    if value
        .strip_prefix('"')
        .and_then(|v| v.strip_suffix('"'))
        .and_then(|v| v.parse::<i64>().ok())
        .is_none_or(|v| v <= 0)
    {
        return Err(Failure::new("if_match_must_be_quoted_positive_revision"));
    }
    Ok(())
}

fn parse_tracking_carrier(value: &str) -> Result<Option<Carrier>> {
    if value.eq_ignore_ascii_case("auto") {
        return Ok(None);
    }
    if value.eq_ignore_ascii_case("fedex") {
        return Ok(Some(Carrier::FedExDefault));
    }
    if value.eq_ignore_ascii_case("fedexaccount") {
        return Ok(Some(Carrier::FedEx));
    }
    value
        .parse()
        .map(Some)
        .map_err(|_| Failure::new("unsupported_tracking_carrier"))
}

async fn pickup_reference(
    api: &Api,
    reference: &str,
    carrier: Option<PickupCarrier>,
) -> Result<Output<PickupRecord>> {
    let mut request = api
        .request(Method::GET, "pickups/resolve")?
        .query(&[("reference", reference)]);
    if let Some(carrier) = carrier {
        request = request.query(&[("carrier", carrier.as_str())]);
    }
    api.fetch(request).await
}

async fn pickup_command(api: &Api, command: PickupCommand) -> Result<()> {
    match command {
        PickupCommand::Carriers => {
            api.output::<Vec<CarrierStatus>>(api.request(Method::GET, "pickups/carriers")?)
                .await
        }
        PickupCommand::Locations { command } => match command {
            LocationCommand::List => {
                api.output::<Vec<PickupSite>>(api.request(Method::GET, "pickups/locations")?)
                    .await
            }
            LocationCommand::Create { input } => {
                let input: packages::pickup::models::SaveSite = read_input(&input.input)?;
                api.output::<PickupSite>(
                    api.request(Method::POST, "pickups/locations")?.json(&input),
                )
                .await
            }
            LocationCommand::Update {
                id,
                if_match,
                input,
            } => {
                validate_etag(&if_match)?;
                let input: packages::pickup::models::SaveSite = read_input(&input.input)?;
                api.output::<PickupSite>(
                    api.request(Method::PUT, &format!("pickups/locations/{id}"))?
                        .header(header::IF_MATCH, checked_header(&if_match)?)
                        .json(&input),
                )
                .await
            }
        },
        PickupCommand::Parcel { command } => match command {
            ParcelCommand::Get { tracking, carrier } => {
                let tracking = packages::pickup::models::normalize_tracking(&tracking)
                    .map_err(|_| Failure::new("invalid_tracking_number"))?;
                let mut request =
                    api.request(Method::GET, &format!("pickups/parcels/{tracking}"))?;
                if let Some(carrier) = carrier {
                    request = request.query(&[("carrier", carrier.as_str())]);
                }
                api.output::<Option<PickupParcel>>(request).await
            }
            ParcelCommand::Set {
                tracking,
                carrier,
                service,
                weight_lb,
                destination_country,
            } => {
                let tracking = packages::pickup::models::normalize_tracking(&tracking)
                    .map_err(|_| Failure::new("invalid_tracking_number"))?;
                let input = ParcelInput {
                    carrier,
                    service,
                    weight_lbs: weight_lb,
                    destination_country,
                };
                input
                    .validate()
                    .map_err(|_| Failure::new("invalid_pickup_parcel_details"))?;
                api.output::<PickupParcel>(
                    api.request(Method::PUT, &format!("pickups/parcels/{tracking}"))?
                        .json(&input),
                )
                .await
            }
        },
        PickupCommand::Availability {
            carrier,
            site,
            date,
            ready_time,
            close_time,
            service,
            destination_country,
        } => {
            let input = ScheduleRequest {
                carrier: Some(carrier),
                site_id: site,
                date,
                ready_time,
                close_time,
                service,
                destination_country,
                ..Default::default()
            };
            api.output::<PickupOffer>(
                api.request(Method::POST, "pickups/availability")?
                    .json(&input),
            )
            .await
        }
        PickupCommand::Preview { plan } => {
            api.output::<Vec<PickupPreview>>(
                api.request(Method::POST, "pickups/preview")?
                    .json(&plan.request(false)),
            )
            .await
        }
        PickupCommand::Schedule {
            plan,
            accept_fee,
            idempotency_key,
        } => {
            let input = plan.request(accept_fee);
            let key = idempotency_key.unwrap_or_else(|| Uuid::now_v7().to_string());
            let result = api
                .fetch::<PickupBatch>(
                    api.request(Method::POST, "pickups")?
                        .header("Idempotency-Key", checked_header(&key)?)
                        .json(&input),
                )
                .await?;
            wait_pickup_batch(api, result).await
        }
        PickupCommand::List { page } => {
            api.output::<PageResponse<PickupRecord>>(page_query(
                api.request(Method::GET, "pickups")?,
                &page,
            ))
            .await
        }
        PickupCommand::Get { reference, carrier } => {
            write_json(&pickup_reference(api, &reference, carrier).await?)
        }
        PickupCommand::Refresh {
            reference,
            carrier,
            if_match,
        } => {
            let record = pickup_reference(api, &reference, carrier).await?;
            let etag = if_match
                .as_deref()
                .or(record.etag.as_deref())
                .ok_or_else(|| Failure::new("missing_response_etag"))?;
            validate_etag(etag)?;
            api.output::<PickupRecord>(
                api.request(
                    Method::POST,
                    &format!("pickups/records/{}/refresh", record.data.id),
                )?
                .header(header::IF_MATCH, checked_header(etag)?),
            )
            .await
        }
        PickupCommand::Reconcile {
            reference,
            carrier,
            if_match,
            input,
        } => {
            let record = pickup_reference(api, &reference, carrier).await?;
            let etag = if_match
                .or(record.etag)
                .ok_or_else(|| Failure::new("missing_etag"))?;
            validate_etag(&etag)?;
            let body: packages::pickup::models::ReconcileRequest = read_input(&input.input)?;
            api.output::<PickupRecord>(
                api.request(
                    Method::POST,
                    &format!("pickups/records/{}/reconcile", record.data.id),
                )?
                .header(header::IF_MATCH, checked_header(&etag)?)
                .json(&body),
            )
            .await
        }
        PickupCommand::Cancel {
            reference,
            carrier,
            if_match,
            idempotency_key,
        } => {
            let record = pickup_reference(api, &reference, carrier).await?;
            let etag = if_match
                .as_deref()
                .or(record.etag.as_deref())
                .ok_or_else(|| Failure::new("missing_response_etag"))?;
            validate_etag(etag)?;
            let key = idempotency_key.unwrap_or_else(|| Uuid::now_v7().to_string());
            let record = api
                .fetch::<PickupRecord>(
                    api.request(
                        Method::POST,
                        &format!("pickups/records/{}/cancel", record.data.id),
                    )?
                    .header(header::IF_MATCH, checked_header(etag)?)
                    .header("Idempotency-Key", checked_header(&key)?),
                )
                .await?;
            wait_pickup_cancel(api, record).await
        }
    }
}

fn pending(state: PickupState) -> bool {
    matches!(
        state,
        PickupState::Pending
            | PickupState::Dispatching
            | PickupState::CancelPending
            | PickupState::Cancelling
    )
}

async fn wait_pickup_batch(api: &Api, mut result: Output<PickupBatch>) -> Result<()> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(300);
    while result.data.items.iter().any(|r| pending(r.state)) {
        tokio::select! {_ = tokio::signal::ctrl_c()=>{write_json(&result)?;return Err(Failure::new("pickup_observation_interrupted_rerun_tracking_command_to_check"));}, _ = tokio::time::sleep(Duration::from_secs(1))=>{}}
        if tokio::time::Instant::now() >= deadline {
            write_json(&result)?;
            return Err(Failure::new(
                "pickup_outcome_not_final_rerun_tracking_command_to_check",
            ));
        }
        result = match api
            .fetch::<PickupBatch>(
                api.request(Method::GET, &format!("pickups/batches/{}", result.data.id))?,
            )
            .await
        {
            Ok(next) => next,
            Err(error) => {
                write_json(&result)?;
                return Err(error);
            }
        };
    }
    let complete = result
        .data
        .items
        .iter()
        .all(|r| r.state == PickupState::Scheduled);
    write_json(&result)?;
    if complete {
        Ok(())
    } else {
        Err(Failure::new(
            "pickup_not_fully_confirmed_see_group_receipts",
        ))
    }
}

async fn wait_pickup_cancel(api: &Api, mut result: Output<PickupRecord>) -> Result<()> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(180);
    while pending(result.data.state) {
        tokio::select! {_ = tokio::signal::ctrl_c()=>{write_json(&result)?;return Err(Failure::new("pickup_cancellation_observation_interrupted_check_tracking_reference"));}, _ = tokio::time::sleep(Duration::from_secs(1))=>{}}
        if tokio::time::Instant::now() >= deadline {
            write_json(&result)?;
            return Err(Failure::new(
                "pickup_cancellation_not_final_check_tracking_reference",
            ));
        }
        result = match api
            .fetch::<PickupRecord>(
                api.request(Method::GET, &format!("pickups/records/{}", result.data.id))?,
            )
            .await
        {
            Ok(next) => next,
            Err(error) => {
                write_json(&result)?;
                return Err(error);
            }
        };
    }
    let complete = result.data.state == PickupState::Cancelled;
    write_json(&result)?;
    if complete {
        Ok(())
    } else {
        Err(Failure::new(
            "pickup_cancellation_not_confirmed_see_receipt",
        ))
    }
}

fn read_input<T: DeserializeOwned>(path: &str) -> Result<T> {
    let input: Box<dyn Read> = if path == "-" {
        Box::new(io::stdin())
    } else {
        Box::new(File::open(path).map_err(|_| Failure::new("input_open_failed"))?)
    };
    let mut bytes = Vec::new();
    input
        .take(MAX_INPUT + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| Failure::new("input_read_failed"))?;
    if bytes.len() as u64 > MAX_INPUT {
        return Err(Failure::new("input_too_large"));
    }
    serde_json::from_slice(&bytes).map_err(|_| Failure::new("invalid_json_input"))
}

fn write_json<T: Serialize>(value: &T) -> Result<()> {
    let mut stdout = io::stdout().lock();
    serde_json::to_writer(&mut stdout, value).map_err(|_| Failure::new("output_failed"))?;
    writeln!(stdout).map_err(|_| Failure::new("output_failed"))
}

fn page_query(request: reqwest::RequestBuilder, page: &Page) -> reqwest::RequestBuilder {
    let mut request = request;
    if let Some(limit) = page.limit {
        request = request.query(&[("limit", limit)]);
    }
    if let Some(token) = &page.page_token {
        request = request.query(&[("page_token", token)]);
    }
    request
}

async fn run(cli: Cli) -> Result<()> {
    let api = Api::new(cli.transport)?;
    if api.home_assistant
        && matches!(
            cli.command,
            Command::Events { .. }
                | Command::Installation
                | Command::CheckInstallation
                | Command::Agents
                | Command::CreateAgent { .. }
                | Command::RevokeAgent { .. }
        )
    {
        return Err(Failure::new(
            "operation_not_exposed_by_home_assistant_proxy",
        ));
    }
    match cli.command {
        Command::List { input, page } => {
            if page.page_token.is_some() && input.is_some() {
                return Err(Failure::new("page_token_cannot_be_combined_with_filters"));
            }
            let filters: Filters = input
                .map(|p| read_input(&p))
                .transpose()?
                .unwrap_or_default();
            api.output::<PageResponse<Shipment>>(page_query(
                api.request(Method::GET, "packages")?.query(&filters),
                &page,
            ))
            .await
        }
        Command::Get { id, carrier } => {
            if carrier.is_none()
                && let Ok(id) = Uuid::parse_str(&id)
            {
                api.output::<Shipment>(api.request(Method::GET, &format!("packages/{id}"))?)
                    .await
            } else {
                write_json(&api.resolve(&id, carrier.as_deref()).await?)
            }
        }
        Command::Create { args } => {
            let input = if let Some(tracking) = args.tracking {
                let carrier = args
                    .carrier
                    .as_deref()
                    .map(parse_tracking_carrier)
                    .transpose()?
                    .flatten();
                CreateShipment {
                    title: args.title.unwrap_or_else(|| "Package".into()),
                    direction: match args.direction.unwrap_or(CliDirection::Inbound) {
                        CliDirection::Inbound => Direction::Inbound,
                        CliDirection::Outbound => Direction::Outbound,
                    },
                    merchant: None,
                    order_reference: None,
                    signature_required: None,
                    tracking: InputTracking {
                        carrier,
                        number: tracking,
                        recipient_url: None,
                    },
                    source: None,
                }
            } else {
                read_input::<CreateShipment>(args.input.as_deref().unwrap_or("-"))?
            };
            let key = args
                .idempotency_key
                .unwrap_or_else(|| Uuid::now_v7().to_string());
            api.output::<Shipment>(
                api.request(Method::POST, "packages")?
                    .header("Idempotency-Key", checked_header(&key)?)
                    .json(&input),
            )
            .await
        }
        Command::Patch { existing, input } => {
            let input: UpdateShipment = read_input(&input.input)?;
            api.output::<Shipment>(
                api.existing(Method::PATCH, &existing, "")
                    .await?
                    .json(&input),
            )
            .await
        }
        Command::AddSource { existing, input } => {
            let input: Source = read_input(&input.input)?;
            api.output::<Shipment>(
                api.existing(Method::POST, &existing, "/sources")
                    .await?
                    .json(&input),
            )
            .await
        }
        Command::SetOverride { existing, input } => {
            let input: SetOverride = read_input(&input.input)?;
            api.output::<Shipment>(
                api.existing(Method::PUT, &existing, "/status-override")
                    .await?
                    .json(&input),
            )
            .await
        }
        Command::ClearOverride { existing } => {
            api.output::<Shipment>(
                api.existing(Method::DELETE, &existing, "/status-override")
                    .await?,
            )
            .await
        }
        Command::Archive { existing } => {
            api.output::<Shipment>(api.existing(Method::PUT, &existing, "/archive").await?)
                .await
        }
        Command::Restore { existing } => {
            api.output::<Shipment>(api.existing(Method::DELETE, &existing, "/archive").await?)
                .await
        }
        Command::Refresh { existing } => {
            api.output::<Shipment>(api.existing(Method::POST, &existing, "/refresh").await?)
                .await
        }
        Command::History { id, carrier, page } => {
            let id = api.package_id(&id, carrier.as_deref()).await?;
            api.output::<PageResponse<Event>>(page_query(
                api.request(Method::GET, &format!("packages/{id}/events"))?,
                &page,
            ))
            .await
        }
        Command::DeliveryEstimates { page } => {
            api.output::<PageResponse<DeliveryEstimate>>(page_query(
                api.request(Method::GET, "delivery-estimates")?,
                &page,
            ))
            .await
        }
        Command::Events { after, limit } => {
            let mut request = api.request(Method::GET, "events")?;
            if let Some(after) = after {
                request = request.query(&[("after", after)]);
            }
            if let Some(limit) = limit {
                request = request.query(&[("limit", limit)]);
            }
            api.output::<StateEventPage>(request).await
        }
        Command::Summary { input } => {
            let query: SummaryQuery = input
                .map(|p| read_input(&p))
                .transpose()?
                .unwrap_or_default();
            api.output::<Summary>(api.request(Method::GET, "summary")?.query(&query))
                .await
        }
        Command::Carriers => {
            api.output::<Vec<Carrier>>(api.request(Method::GET, "carriers")?)
                .await
        }
        Command::System => {
            api.output::<System>(api.request(Method::GET, "system")?)
                .await
        }
        Command::Installation => {
            api.output::<InstallationReport>(api.request(Method::GET, "installation")?)
                .await
        }
        Command::CheckInstallation => {
            api.output::<InstallationReport>(api.request(Method::POST, "installation/check")?)
                .await
        }
        Command::Agents => {
            api.output::<AgentCredentials>(api.request(Method::GET, "agents")?)
                .await
        }
        Command::CreateAgent { input } => {
            let input: CreateAgent = read_input(&input.input)?;
            api.output::<CreatedAgentCredential>(api.request(Method::POST, "agents")?.json(&input))
                .await
        }
        Command::RevokeAgent { id } => api.revoke(id).await,
        Command::Pickups { command } => pickup_command(&api, command).await,
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(error)
            if matches!(
                error.kind(),
                clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion
            ) =>
        {
            return if error.print().is_ok() {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            };
        }
        // Clap diagnostics can echo arguments. Keep even usage failures private.
        Err(_) => {
            eprintln!("{{\"error\":\"invalid_arguments_use_help\"}}");
            return ExitCode::from(2);
        }
    };
    match run(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            let _ = serde_json::to_writer(io::stderr().lock(), &error);
            eprintln!();
            ExitCode::FAILURE
        }
    }
}
