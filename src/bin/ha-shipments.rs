//! A transport-only client. The server owns validation, authorization and tracking.
use clap::{Args, Parser, Subcommand, ValueEnum};
use reqwest::{Client, Method, Url, header};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use shipments::{carrier::Carrier, installation::InstallationReport, models::*};
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
    name = "ha-shipments",
    version,
    about = "JSON client for the Shipments API",
    long_about = "JSON client for the Shipments API. Configure SHIPMENTS_URL and SHIPMENTS_API_TOKEN (or SHIPMENTS_AGENT_TOKEN), or HOME_ASSISTANT_URL and HOME_ASSISTANT_TOKEN. Never retries or follows redirects. Each paginated command makes one request."
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
    id: Uuid,
    /// Exact strong ETag from the previous response, including double quotes.
    #[arg(long, allow_hyphen_values = true)]
    if_match: String,
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
    Get {
        id: Uuid,
    },
    Create {
        #[command(flatten)]
        input: Input,
        #[arg(long)]
        idempotency_key: String,
    },
    Patch {
        #[command(flatten)]
        existing: Existing,
        #[command(flatten)]
        input: Input,
    },
    AddSource {
        #[command(flatten)]
        existing: Existing,
        #[command(flatten)]
        input: Input,
    },
    SetOverride {
        #[command(flatten)]
        existing: Existing,
        #[command(flatten)]
        input: Input,
    },
    ClearOverride {
        #[command(flatten)]
        existing: Existing,
    },
    Archive {
        #[command(flatten)]
        existing: Existing,
    },
    Restore {
        #[command(flatten)]
        existing: Existing,
    },
    Refresh {
        #[command(flatten)]
        existing: Existing,
    },
    History {
        id: Uuid,
        #[command(flatten)]
        page: Page,
    },
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
    Summary {
        #[arg(long)]
        input: Option<String>,
    },
    Carriers,
    System,
    /// Owner-only native API operations, not exposed by the HA proxy.
    Installation,
    CheckInstallation,
    Agents,
    CreateAgent {
        #[command(flatten)]
        input: Input,
    },
    RevokeAgent {
        id: Uuid,
    },
}

// No provider error strings, URLs, credentials, response bodies or input values
// cross the stderr boundary, even when a remote endpoint is malicious.
#[derive(Serialize)]
struct Failure {
    #[serde(skip_serializing_if = "Option::is_none")]
    error_code: Option<ApiErrorCode>,
    error: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    status: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    retry_after_seconds: Option<u64>,
}
impl Failure {
    fn new(error: &'static str) -> Self {
        Self {
            error,
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
}

#[derive(Deserialize)]
struct ErrorResponse {
    error_code: Option<ApiErrorCode>,
}

async fn http_failure(mut response: reqwest::Response, error: &'static str) -> Failure {
    let mut failure = Failure {
        error,
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
        let native = variable("SHIPMENTS_URL")?;
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
            "/api/shipments/v1/"
        } else {
            "/v1/"
        });
        let token = if home_assistant {
            variable("HOME_ASSISTANT_TOKEN")?
        } else {
            match (
                variable("SHIPMENTS_API_TOKEN")?,
                variable("SHIPMENTS_AGENT_TOKEN")?,
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

    fn existing(
        &self,
        method: Method,
        existing: &Existing,
        suffix: &str,
    ) -> Result<reqwest::RequestBuilder> {
        let value = &existing.if_match;
        // Match the API's single quoted positive i64 revision contract. Never
        // fetch the revision automatically, which would defeat optimistic locking.
        if value
            .strip_prefix('"')
            .and_then(|v| v.strip_suffix('"'))
            .and_then(|v| v.parse::<i64>().ok())
            .is_none_or(|v| v <= 0)
        {
            return Err(Failure::new("if_match_must_be_quoted_positive_revision"));
        }
        Ok(self
            .request(method, &format!("shipments/{}{suffix}", existing.id))?
            .header(header::IF_MATCH, checked_header(value)?))
    }

    async fn output<T: DeserializeOwned + Serialize>(
        &self,
        request: reqwest::RequestBuilder,
    ) -> Result<()> {
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
        write_json(&Output {
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
                api.request(Method::GET, "shipments")?.query(&filters),
                &page,
            ))
            .await
        }
        Command::Get { id } => {
            api.output::<Shipment>(api.request(Method::GET, &format!("shipments/{id}"))?)
                .await
        }
        Command::Create {
            input,
            idempotency_key,
        } => {
            let input: CreateShipment = read_input(&input.input)?;
            api.output::<Shipment>(
                api.request(Method::POST, "shipments")?
                    .header("Idempotency-Key", checked_header(&idempotency_key)?)
                    .json(&input),
            )
            .await
        }
        Command::Patch { existing, input } => {
            let input: UpdateShipment = read_input(&input.input)?;
            api.output::<Shipment>(api.existing(Method::PATCH, &existing, "")?.json(&input))
                .await
        }
        Command::AddSource { existing, input } => {
            let input: Source = read_input(&input.input)?;
            api.output::<Shipment>(
                api.existing(Method::POST, &existing, "/sources")?
                    .json(&input),
            )
            .await
        }
        Command::SetOverride { existing, input } => {
            let input: SetOverride = read_input(&input.input)?;
            api.output::<Shipment>(
                api.existing(Method::PUT, &existing, "/status-override")?
                    .json(&input),
            )
            .await
        }
        Command::ClearOverride { existing } => {
            api.output::<Shipment>(api.existing(Method::DELETE, &existing, "/status-override")?)
                .await
        }
        Command::Archive { existing } => {
            api.output::<Shipment>(api.existing(Method::PUT, &existing, "/archive")?)
                .await
        }
        Command::Restore { existing } => {
            api.output::<Shipment>(api.existing(Method::DELETE, &existing, "/archive")?)
                .await
        }
        Command::Refresh { existing } => {
            api.output::<Shipment>(api.existing(Method::POST, &existing, "/refresh")?)
                .await
        }
        Command::History { id, page } => {
            api.output::<PageResponse<Event>>(page_query(
                api.request(Method::GET, &format!("shipments/{id}/events"))?,
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
