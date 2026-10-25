//! AfterShip Tracking 2026-07. Registration always reconciles before its single
//! write; provider failures never retain tracking numbers or response text.

use std::{collections::HashSet, net::IpAddr, sync::Arc, time::Duration};

use reqwest::{Client, RequestBuilder, Response, Url};
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, OwnedMutexGuard};
use tokio::time::Instant;
use uuid::Uuid;

use crate::{
    carrier::Carrier,
    models::{
        CarrierEvent, CarrierTracking, ProviderValue, Status, TrackingLocation, provider_optional,
    },
    shipping_api::{
        PreparedRegistration, ShippingApi,
        error::{Error as SyncError, ErrorKind, Rejection},
        types::{ApiFuture, ProviderId, Registration, Tracker},
    },
};

const BASE: &str = "https://api.aftership.com/tracking/2026-07/";
const MAX_BODY_BYTES: usize = 256 * 1024;
const MAX_PAGES: usize = 20;
const MAX_RETRY_SECONDS: i64 = 6 * 60 * 60;
// AfterShip retains tracking records for 120 days. An empty lookup beyond that
// window cannot prove that an earlier registration never incurred a charge.
const RETENTION_SECONDS: i64 = 120 * 24 * 60 * 60;

fn provider_error(kind: ErrorKind) -> SyncError {
    SyncError::new(ProviderId::AfterShip, kind)
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
enum Courier {
    // AfterShip identifies US recipient MCF / Swiship parcels as `amazon`
    // (Amazon Shipping + Amazon MCF), distinct from its `amazon-mcf` courier.
    // Detection must still confirm this exact slug before registration.
    #[serde(rename = "amazon")]
    AmazonMcf,
    #[serde(rename = "speedx")]
    SpeedX,
    #[serde(rename = "sway-webhook")]
    Sway,
    #[serde(rename = "veho")]
    Veho,
}

impl Courier {
    fn for_carrier(carrier: Option<Carrier>) -> Result<Self, SyncError> {
        match carrier {
            Some(Carrier::AmazonMcf) => Ok(Self::AmazonMcf),
            Some(Carrier::SpeedX) => Ok(Self::SpeedX),
            Some(Carrier::Sway) => Ok(Self::Sway),
            Some(Carrier::Veho) => Ok(Self::Veho),
            _ => Err(provider_error(ErrorKind::Rejected(
                Rejection::NoCarrierForCode,
            ))),
        }
    }

    fn slug(self) -> &'static str {
        match self {
            Self::AmazonMcf => "amazon",
            Self::SpeedX => "speedx",
            Self::Sway => "sway-webhook",
            Self::Veho => "veho",
        }
    }

    fn carrier(self) -> Carrier {
        match self {
            Self::AmazonMcf => Carrier::AmazonMcf,
            Self::SpeedX => Carrier::SpeedX,
            Self::Sway => Carrier::Sway,
            Self::Veho => Carrier::Veho,
        }
    }
}

#[derive(Clone, Copy, Deserialize)]
enum Tag {
    Pending,
    InfoReceived,
    InTransit,
    OutForDelivery,
    AttemptFail,
    Delivered,
    AvailableForPickup,
    Exception,
    Expired,
}

impl Tag {
    fn status(self, subtag: &str) -> Status {
        match subtag {
            "Exception_011" => return Status::ReturnToSender,
            "Exception_020" => return Status::Cancelled,
            _ => {}
        }
        match self {
            Self::Pending => Status::Unknown,
            Self::InfoReceived => Status::PreTransit,
            Self::InTransit => Status::InTransit,
            Self::OutForDelivery => Status::OutForDelivery,
            Self::AttemptFail | Self::Exception => Status::Failure,
            Self::Delivered => Status::Delivered,
            Self::AvailableForPickup => Status::AvailableForPickup,
            Self::Expired => Status::Unknown,
        }
    }
}

#[derive(Deserialize)]
struct Envelope<T> {
    meta: Meta,
    data: T,
}

#[derive(Deserialize)]
struct Meta {
    code: u16,
}

#[derive(Deserialize)]
struct ErrorEnvelope {
    meta: Meta,
}

#[derive(Deserialize)]
struct Pagination {
    total: usize,
    next_cursor: Option<String>,
    has_next_page: bool,
}

impl Pagination {
    fn next(self, seen: &mut HashSet<String>, count: usize) -> Result<Option<String>, SyncError> {
        if !self.has_next_page {
            if self
                .next_cursor
                .as_deref()
                .is_some_and(|value| !value.is_empty())
            {
                return Err(provider_error(ErrorKind::InvalidResponse));
            }
            return Ok(None);
        }
        let cursor = self
            .next_cursor
            .filter(|cursor| {
                !cursor.is_empty() && cursor.len() <= 4096 && !cursor.chars().any(char::is_control)
            })
            .ok_or_else(|| provider_error(ErrorKind::InvalidResponse))?;
        if count == 0 || !seen.insert(cursor.clone()) {
            return Err(provider_error(ErrorKind::InvalidResponse));
        }
        Ok(Some(cursor))
    }
}

#[derive(Deserialize)]
struct HealthPage {
    trackings: Vec<HealthTracker>,
}

#[derive(Deserialize)]
struct HealthTracker {
    id: String,
}

#[derive(Deserialize)]
struct TrackingPage {
    pagination: Pagination,
    trackings: Vec<ProviderTracker>,
}

#[derive(Deserialize)]
struct Couriers {
    total: usize,
    couriers: Vec<CourierIdentity>,
}

#[derive(Deserialize)]
struct CourierIdentity {
    slug: Courier,
}

#[derive(Deserialize)]
struct ConnectionPage {
    pagination: Pagination,
    courier_connections: Vec<Connection>,
}

#[derive(Deserialize)]
struct Connection {
    id: String,
    courier_slug: Courier,
}

#[derive(Deserialize)]
struct ProviderTracker {
    id: String,
    tracking_number: String,
    slug: Courier,
    tag: Tag,
    subtag: String,
    #[serde(default, deserialize_with = "provider_optional")]
    checkpoints: Option<Vec<ProviderValue<Checkpoint>>>,
    #[serde(default, deserialize_with = "provider_optional")]
    signed_by: Option<String>,
    #[serde(default, deserialize_with = "provider_optional")]
    shipment_delivery_date: Option<String>,
    #[serde(default, deserialize_with = "provider_optional")]
    courier_estimated_delivery_date: Option<Estimate>,
    #[serde(default, deserialize_with = "provider_optional")]
    aftership_estimated_delivery_date: Option<Estimate>,
    #[serde(default, deserialize_with = "provider_optional")]
    aftership_tracking_url: Option<String>,
}

#[derive(Deserialize)]
struct Estimate {
    #[serde(default, deserialize_with = "provider_optional")]
    estimated_delivery_date: Option<String>,
}

#[derive(Deserialize)]
struct Checkpoint {
    tag: Tag,
    #[serde(default, deserialize_with = "provider_optional")]
    subtag: Option<String>,
    #[serde(default, deserialize_with = "provider_optional")]
    checkpoint_time: Option<String>,
    #[serde(default, deserialize_with = "provider_optional")]
    message: Option<String>,
    #[serde(default, deserialize_with = "provider_optional")]
    source: Option<CheckpointSource>,
    #[serde(default, deserialize_with = "provider_optional")]
    raw_tag: Option<String>,
    #[serde(default, deserialize_with = "provider_optional")]
    city: Option<String>,
    #[serde(default, deserialize_with = "provider_optional")]
    state: Option<String>,
    #[serde(default, deserialize_with = "provider_optional")]
    postal_code: Option<String>,
    #[serde(default, deserialize_with = "provider_optional")]
    country_region: Option<String>,
}

#[derive(Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum CheckpointSource {
    Carrier,
    User,
}

impl ProviderTracker {
    fn normalize(self, number: &str, expected: Option<Courier>) -> Result<Tracker, SyncError> {
        if !valid_id(&self.id)
            || self.tracking_number != number
            || expected.is_some_and(|expected| expected != self.slug)
        {
            return Err(provider_error(ErrorKind::InvalidResponse));
        }
        match self.subtag.as_str() {
            "Pending_002" => {
                return Err(provider_error(ErrorKind::ProviderStateRejected {
                    rejection: Rejection::CarrierCredentialsMissing,
                    code: "Pending_002",
                }));
            }
            "Pending_004" => {
                return Err(provider_error(ErrorKind::ProviderStateRejected {
                    rejection: Rejection::CarrierCodeMismatch,
                    code: "Pending_004",
                }));
            }
            "Pending_006" => {
                return Err(provider_error(ErrorKind::ProviderStateRejected {
                    rejection: Rejection::NoCarrierForCode,
                    code: "Pending_006",
                }));
            }
            _ => {}
        }
        let status = self.tag.status(&self.subtag);
        let mut tracking = CarrierTracking {
            signed_by: clean_text(self.signed_by, 200),
            estimated_delivery_at: self
                .courier_estimated_delivery_date
                .and_then(|estimate| clean_datetime(estimate.estimated_delivery_date))
                .or_else(|| {
                    self.aftership_estimated_delivery_date
                        .and_then(|estimate| clean_datetime(estimate.estimated_delivery_date))
                }),
            ..Default::default()
        };
        for raw in self.checkpoints.unwrap_or_default() {
            let ProviderValue::Value(checkpoint) = raw else {
                continue;
            };
            // A completion entered by an AfterShip user is not a carrier scan.
            if checkpoint.source == Some(CheckpointSource::User) {
                continue;
            }
            let location = TrackingLocation {
                city: clean_text(checkpoint.city, 200),
                state: clean_text(checkpoint.state, 100),
                zip: clean_text(checkpoint.postal_code, 40),
                country: clean_text(checkpoint.country_region, 100),
            };
            tracking.events.push(CarrierEvent {
                status: Some(
                    checkpoint
                        .tag
                        .status(checkpoint.subtag.as_deref().unwrap_or_default()),
                ),
                status_detail: clean_text(checkpoint.subtag, 100),
                message: clean_text(checkpoint.message, 1024),
                // Never replace a missing/naive carrier time with created_at:
                // that is the import time, not when the package moved.
                datetime: clean_datetime(checkpoint.checkpoint_time),
                source: Some(self.slug.carrier().to_string()),
                carrier_code: clean_text(checkpoint.raw_tag, 100),
                tracking_location: (location != TrackingLocation::default()).then_some(location),
                description: None,
            });
        }
        let incoming = std::mem::take(&mut tracking);
        tracking.merge(&incoming);
        if tracking.delivered_at.is_none() && status == Status::Delivered {
            tracking.delivered_at = clean_datetime(self.shipment_delivery_date);
        }
        Ok(Tracker {
            provider: ProviderId::AfterShip,
            id: self.id,
            tracking_code: self.tracking_number,
            carrier: self.slug.carrier(),
            status,
            public_url: self.aftership_tracking_url.and_then(safe_tracking_url),
            carrier_tracking: tracking,
        })
    }
}

#[derive(Serialize)]
struct Detection<'a> {
    tracking_number: &'a str,
    slug: [Courier; 1],
}

#[derive(Serialize)]
struct NewTracking<'a> {
    tracking_number: &'a str,
    slug: Courier,
    #[serde(skip_serializing_if = "Option::is_none")]
    courier_connection_id: Option<&'a str>,
}

#[derive(Clone)]
pub struct AfterShipClient {
    client: Client,
    api_key: reqwest::header::HeaderValue,
    base: Url,
    requests: Arc<Mutex<RequestSchedule>>,
    log: slog::Logger,
    request_id: Option<String>,
}

struct RequestSchedule {
    next_start: Instant,
    cooldown: Option<Instant>,
    rate_limits: u32,
}

struct ProviderResponse {
    bytes: Vec<u8>,
    request_id: Option<Uuid>,
}

impl ProviderResponse {
    fn attach(&self, mut error: SyncError) -> SyncError {
        error.provider_request_id = self.request_id;
        error
    }
}

impl AfterShipClient {
    pub fn new(api_key: impl Into<String>) -> Result<Self, SyncError> {
        Self::build(api_key.into(), BASE)
    }

    /// Test transport is restricted to a literal loopback address. Production
    /// credentials never acquire a user-configurable destination.
    pub fn for_loopback(api_key: impl Into<String>, base_url: &str) -> Result<Self, SyncError> {
        let url = Url::parse(base_url).map_err(|_| provider_error(ErrorKind::Configuration))?;
        let loopback = url
            .host_str()
            .and_then(|host| host.trim_matches(['[', ']']).parse::<IpAddr>().ok())
            .is_some_and(|address| address.is_loopback());
        if !loopback || !matches!(url.scheme(), "http" | "https") {
            return Err(provider_error(ErrorKind::Configuration));
        }
        Self::build(api_key.into(), base_url)
    }

    fn build(api_key: String, base_url: &str) -> Result<Self, SyncError> {
        let base = Url::parse(base_url).map_err(|_| provider_error(ErrorKind::Configuration))?;
        if api_key.trim().is_empty()
            || !base.username().is_empty()
            || base.password().is_some()
            || base.query().is_some()
            || base.fragment().is_some()
            || !base.path().ends_with('/')
        {
            return Err(provider_error(ErrorKind::Configuration));
        }
        let mut api_key = reqwest::header::HeaderValue::from_str(&api_key)
            .map_err(|_| provider_error(ErrorKind::Configuration))?;
        api_key.set_sensitive(true);
        let client = Client::builder()
            .timeout(Duration::from_secs(20))
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .user_agent(concat!("shipments/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|_| provider_error(ErrorKind::Configuration))?;
        Ok(Self {
            client,
            api_key,
            base,
            requests: Arc::new(Mutex::new(RequestSchedule {
                next_start: Instant::now(),
                cooldown: None,
                rate_limits: 0,
            })),
            log: crate::logging::logger(),
            request_id: None,
        })
    }

    pub fn with_logger(mut self, log: slog::Logger) -> Self {
        self.log = log;
        self
    }

    fn url(&self, path: &str) -> Result<Url, SyncError> {
        self.base
            .join(path)
            .map_err(|_| provider_error(ErrorKind::Configuration))
    }

    fn authenticated(&self, request: RequestBuilder) -> RequestBuilder {
        request
            .header("as-api-key", &self.api_key)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
    }

    async fn observe<T>(
        &self,
        operation: &'static str,
        future: impl std::future::Future<Output = Result<T, SyncError>>,
    ) -> Result<T, SyncError> {
        let started = Instant::now();
        let result = future.await;
        let error = result.as_ref().err();
        slog::info!(self.log, "provider_request";
            "request_id" => self.request_id.clone().unwrap_or_else(|| Uuid::now_v7().to_string()),
            "operation" => operation, "outcome" => if error.is_some() { "error" } else { "success" },
            "error_code" => error.map(SyncError::error_code), "provider_status" => error.and_then(SyncError::provider_status),
            "provider_error_code" => error.and_then(SyncError::provider_error_code),
            "provider_request_id" => error.and_then(|error| error.provider_request_id).map(|id| id.to_string()),
            "retry_after_seconds" => error.and_then(SyncError::retry_after),
            "provider" => ProviderId::AfterShip.as_str(), "duration_ms" => started.elapsed().as_millis());
        result
    }

    async fn slot(&self) -> Result<OwnedMutexGuard<RequestSchedule>, SyncError> {
        let schedule = self
            .requests
            .clone()
            .try_lock_owned()
            .map_err(|_| provider_error(ErrorKind::Throttled { retry_after: 1 }))?;
        if let Some(until) = schedule.cooldown.filter(|until| *until > Instant::now()) {
            return Err(provider_error(ErrorKind::Throttled {
                retry_after: i64::try_from(
                    until.saturating_duration_since(Instant::now()).as_secs(),
                )
                .unwrap_or(MAX_RETRY_SECONDS)
                    + 1,
            }));
        }
        tokio::time::sleep_until(schedule.next_start).await;
        Ok(schedule)
    }

    async fn send(&self, request: RequestBuilder) -> Result<ProviderResponse, SyncError> {
        let slot = self.slot().await?;
        self.send_in_slot(request, slot).await
    }

    async fn send_in_slot(
        &self,
        request: RequestBuilder,
        mut slot: OwnedMutexGuard<RequestSchedule>,
    ) -> Result<ProviderResponse, SyncError> {
        slot.next_start = Instant::now() + Duration::from_secs(1);
        let result = match request.send().await {
            Ok(response) => {
                let request_id = response
                    .headers()
                    .get("x-request-id")
                    .and_then(|value| value.to_str().ok())
                    .and_then(|value| Uuid::parse_str(value).ok());
                Self::read_body(response)
                    .await
                    .map(|bytes| ProviderResponse { bytes, request_id })
                    .map_err(|mut error| {
                        error.provider_request_id = request_id;
                        error
                    })
            }
            Err(error) => Err(provider_error(if error.is_timeout() {
                ErrorKind::Timeout
            } else {
                ErrorKind::Connection
            })),
        };
        match result {
            Err(mut error) if error.provider_status() == Some(429) => {
                slot.rate_limits = slot.rate_limits.saturating_add(1);
                let delay = 60_i64
                    .saturating_mul(2_i64.saturating_pow(slot.rate_limits.saturating_sub(1)))
                    .clamp(60, MAX_RETRY_SECONDS)
                    .max(error.retry_after().unwrap_or(0));
                slot.cooldown = Some(Instant::now() + Duration::from_secs(delay.unsigned_abs()));
                error.set_retry_after(delay);
                Err(error)
            }
            Err(error) if error.retry_after().is_some() => {
                if let Some(delay) = error.retry_after() {
                    slot.cooldown =
                        Some(Instant::now() + Duration::from_secs(delay.unsigned_abs()));
                }
                Err(error)
            }
            Ok(body) => {
                slot.rate_limits = 0;
                slot.cooldown = None;
                Ok(body)
            }
            Err(error) => Err(error),
        }
    }

    async fn read_body(mut response: Response) -> Result<Vec<u8>, SyncError> {
        let status = response.status().as_u16();
        let retry_after = response
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<i64>().ok())
            .filter(|seconds| (1..=MAX_RETRY_SECONDS).contains(seconds));
        let http_error = || {
            provider_error(ErrorKind::Http {
                status,
                retry_after,
            })
        };
        if response
            .content_length()
            .is_some_and(|length| length > MAX_BODY_BYTES as u64)
        {
            return Err(if !(200..300).contains(&status) {
                http_error()
            } else {
                provider_error(ErrorKind::ResponseTooLarge)
            });
        }
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|error| {
            if !(200..300).contains(&status) {
                http_error()
            } else {
                provider_error(if error.is_timeout() {
                    ErrorKind::Timeout
                } else {
                    ErrorKind::Connection
                })
            }
        })? {
            if chunk.len() > MAX_BODY_BYTES.saturating_sub(body.len()) {
                return Err(if !(200..300).contains(&status) {
                    http_error()
                } else {
                    provider_error(ErrorKind::ResponseTooLarge)
                });
            }
            body.extend_from_slice(&chunk);
        }
        if !(200..300).contains(&status) {
            if let Ok(error) = serde_json::from_slice::<ErrorEnvelope>(&body) {
                let rejected = match error.meta.code {
                    4005 => Some((Rejection::InvalidParams, "4005")),
                    4007 => Some((Rejection::InvalidParams, "4007")),
                    4008 => Some((Rejection::InvalidParams, "4008")),
                    4009 => Some((Rejection::InvalidParams, "4009")),
                    4017 => Some((Rejection::InvalidParams, "4017")),
                    4010 => Some((Rejection::NoCarrierForCode, "4010")),
                    4011 => Some((Rejection::CarrierCredentialsMissing, "4011")),
                    4012 => Some((Rejection::NoCarrierForCode, "4012")),
                    _ => None,
                };
                if let Some((rejection, code)) = rejected {
                    return Err(provider_error(ErrorKind::ProviderRejected {
                        rejection,
                        code,
                        status,
                        retry_after,
                    }));
                }
            }
            return Err(http_error());
        }
        Ok(body)
    }

    fn decode<T: for<'de> Deserialize<'de>>(body: &ProviderResponse) -> Result<T, SyncError> {
        let envelope: Envelope<T> = serde_json::from_slice(&body.bytes)
            .map_err(|_| body.attach(provider_error(ErrorKind::InvalidResponse)))?;
        if !matches!(envelope.meta.code, 200 | 201) {
            return Err(body.attach(provider_error(ErrorKind::InvalidResponse)));
        }
        Ok(envelope.data)
    }

    async fn lookup(&self, registration: &Registration) -> Result<Option<Tracker>, SyncError> {
        let courier = Courier::for_carrier(registration.carrier)?;
        let mut cursor: Option<String> = None;
        let mut seen = HashSet::new();
        let mut candidate = None;
        let mut total = None;
        let mut count = 0;
        for _ in 0..MAX_PAGES {
            let mut query = vec![
                ("tracking_numbers", registration.tracking_number.as_str()),
                ("slug", courier.slug()),
                ("limit", "200"),
            ];
            if let Some(cursor) = &cursor {
                query.push(("cursor", cursor));
            }
            let request = self.authenticated(self.client.get(self.url("trackings")?).query(&query));
            let body = self.send(request).await?;
            let page: TrackingPage = Self::decode(&body)?;
            if total.is_some_and(|total| total != page.pagination.total) {
                return Err(provider_error(ErrorKind::InvalidResponse));
            }
            total = Some(page.pagination.total);
            let page_count = page.trackings.len();
            if page_count > 200 {
                return Err(provider_error(ErrorKind::InvalidResponse));
            }
            count += page_count;
            for tracker in page.trackings {
                let tracker = tracker
                    .normalize(&registration.tracking_number, Some(courier))
                    .map_err(|error| body.attach(error))?;
                if candidate.replace(tracker).is_some() {
                    return Err(provider_error(ErrorKind::RegistrationAmbiguous));
                }
            }
            cursor = page.pagination.next(&mut seen, page_count)?;
            if cursor.is_none() {
                if total != Some(count) {
                    return Err(provider_error(ErrorKind::InvalidResponse));
                }
                if candidate.is_none() {
                    check_retention(registration.started_at)?;
                } else if candidate.as_ref().is_some_and(|tracker: &Tracker| {
                    tracker.status == Status::Unknown && tracker.carrier_tracking.events.is_empty()
                }) {
                    self.detect(&registration.tracking_number, courier).await?;
                }
                return Ok(candidate);
            }
        }
        Err(provider_error(ErrorKind::RegistrationAmbiguous))
    }

    async fn detect(&self, number: &str, courier: Courier) -> Result<(), SyncError> {
        let request = self.authenticated(self.client.post(self.url("couriers/detect")?).json(
            &Detection {
                tracking_number: number,
                slug: [courier],
            },
        ));
        let body = self.send(request).await?;
        let detected: Couriers = Self::decode(&body)?;
        if detected.total != detected.couriers.len() || detected.total > 1 {
            return Err(provider_error(ErrorKind::InvalidResponse));
        }
        if detected
            .couriers
            .first()
            .is_none_or(|detected| detected.slug != courier)
        {
            return Err(body.attach(provider_error(ErrorKind::Rejected(
                Rejection::CarrierCodeMismatch,
            ))));
        }
        Ok(())
    }

    async fn sway_connection(&self) -> Result<String, SyncError> {
        let mut cursor: Option<String> = None;
        let mut seen = HashSet::new();
        let mut selected = None;
        let mut count = 0;
        let mut expected_total = None;
        for _ in 0..MAX_PAGES {
            let mut query = vec![("courier_slug", "sway-webhook"), ("limit", "200")];
            if let Some(cursor) = &cursor {
                query.push(("cursor", cursor));
            }
            let request = self.authenticated(
                self.client
                    .get(self.url("courier-connections")?)
                    .query(&query),
            );
            let body = self.send(request).await?;
            let page: ConnectionPage = Self::decode(&body)?;
            let page_count = page.courier_connections.len();
            if page_count > 200
                || expected_total.is_some_and(|total| total != page.pagination.total)
            {
                return Err(provider_error(ErrorKind::InvalidResponse));
            }
            expected_total = Some(page.pagination.total);
            count += page_count;
            let total = page.pagination.total;
            for connection in page.courier_connections {
                if connection.courier_slug != Courier::Sway
                    || !valid_id(&connection.id)
                    || selected.replace(connection.id).is_some()
                {
                    return Err(provider_error(ErrorKind::RegistrationAmbiguous));
                }
            }
            cursor = page.pagination.next(&mut seen, page_count)?;
            if cursor.is_none() {
                if total != count {
                    return Err(provider_error(ErrorKind::InvalidResponse));
                }
                return selected.ok_or_else(|| {
                    body.attach(provider_error(ErrorKind::Rejected(
                        Rejection::CarrierCredentialsMissing,
                    )))
                });
            }
        }
        Err(provider_error(ErrorKind::RegistrationAmbiguous))
    }

    async fn prepare(
        &self,
        registration: Registration,
    ) -> Result<Box<dyn PreparedRegistration>, SyncError> {
        let courier = Courier::for_carrier(registration.carrier)?;
        // Keep the safety rule inside this adapter, including callers other than
        // the worker. Unknown existing registrations never justify another POST.
        if let Some(tracker) = self.lookup(&registration).await? {
            return Ok(Box::new(ExistingRegistration(tracker)));
        }
        self.detect(&registration.tracking_number, courier).await?;
        let connection = if courier == Courier::Sway {
            Some(self.sway_connection().await?)
        } else {
            None
        };
        let slot = self.slot().await?;
        Ok(Box::new(AfterShipRegistration {
            client: self.clone(),
            registration,
            courier,
            connection,
            slot,
        }))
    }

    async fn retrieve_tracker(&self, tracker_id: &str, number: &str) -> Result<Tracker, SyncError> {
        if !valid_id(tracker_id) {
            return Err(provider_error(ErrorKind::InvalidResponse));
        }
        let mut url = self.url("trackings/")?;
        url.path_segments_mut()
            .map_err(|_| provider_error(ErrorKind::Configuration))?
            .pop_if_empty()
            .push(tracker_id);
        let request = self.authenticated(self.client.get(url));
        let body = self.send(request).await?;
        let tracker: ProviderTracker = Self::decode(&body)?;
        let tracker = tracker
            .normalize(number, None)
            .map_err(|error| body.attach(error))?;
        if tracker.id != tracker_id {
            return Err(provider_error(ErrorKind::InvalidResponse));
        }
        Ok(tracker)
    }
}

fn check_retention(started_at: i64) -> Result<(), SyncError> {
    if time::OffsetDateTime::now_utc().unix_timestamp()
        >= started_at.saturating_add(RETENTION_SECONDS)
    {
        return Err(provider_error(ErrorKind::RegistrationExpired));
    }
    Ok(())
}

impl ShippingApi for AfterShipClient {
    fn id(&self) -> ProviderId {
        ProviderId::AfterShip
    }
    fn blocked(&self) -> bool {
        self.requests.try_lock().map_or(true, |schedule| {
            schedule
                .cooldown
                .is_some_and(|until| until > Instant::now())
        })
    }
    fn for_request(&self, request_id: &str, shipment_id: Option<&str>) -> Arc<dyn ShippingApi> {
        let mut client = self.clone();
        client.request_id = Some(request_id.into());
        if let Some(id) = shipment_id {
            client.log = client.log.new(slog::o!("shipment_id" => id.to_owned()));
        }
        Arc::new(client)
    }
    fn check_connection(&self) -> ApiFuture<'_, ()> {
        Box::pin(self.observe("health", async {
            let request = self.authenticated(
                self.client
                    .get(self.url("trackings")?)
                    .query(&[("limit", 1)]),
            );
            let page: HealthPage = Self::decode(&self.send(request).await?)?;
            if page.trackings.len() > 1
                || page.trackings.iter().any(|tracker| !valid_id(&tracker.id))
            {
                return Err(provider_error(ErrorKind::InvalidConnectionResponse));
            }
            Ok(())
        }))
    }
    fn reconcile_registration<'a>(
        &'a self,
        registration: &'a Registration,
    ) -> ApiFuture<'a, Option<Tracker>> {
        Box::pin(self.observe("reconcile", self.lookup(registration)))
    }
    fn prepare_registration(
        &self,
        registration: Registration,
    ) -> ApiFuture<'_, Box<dyn PreparedRegistration>> {
        Box::pin(self.observe("prepare", self.prepare(registration)))
    }
    fn retrieve<'a>(
        &'a self,
        tracker_id: &'a str,
        tracking_number: &'a str,
    ) -> ApiFuture<'a, Tracker> {
        Box::pin(self.observe(
            "retrieve",
            self.retrieve_tracker(tracker_id, tracking_number),
        ))
    }
}

struct ExistingRegistration(Tracker);
impl PreparedRegistration for ExistingRegistration {
    fn register(self: Box<Self>) -> ApiFuture<'static, Tracker> {
        Box::pin(async move { Ok(self.0) })
    }
}

struct AfterShipRegistration {
    client: AfterShipClient,
    registration: Registration,
    courier: Courier,
    connection: Option<String>,
    slot: OwnedMutexGuard<RequestSchedule>,
}

impl PreparedRegistration for AfterShipRegistration {
    fn register(self: Box<Self>) -> ApiFuture<'static, Tracker> {
        Box::pin(async move {
            let Self {
                client,
                registration,
                courier,
                connection,
                slot,
            } = *self;
            check_retention(registration.started_at)?;
            let request = client.authenticated(client.client.post(client.url("trackings")?).json(
                &NewTracking {
                    tracking_number: &registration.tracking_number,
                    slug: courier,
                    courier_connection_id: connection.as_deref(),
                },
            ));
            client
                .observe("register", async {
                    let body = client.send_in_slot(request, slot).await?;
                    let tracker: ProviderTracker = AfterShipClient::decode(&body)?;
                    tracker
                        .normalize(&registration.tracking_number, Some(courier))
                        .map_err(|error| body.attach(error))
                })
                .await
        })
    }
}

fn valid_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn clean_text(value: Option<String>, maximum: usize) -> Option<String> {
    let value = value?;
    let mut cleaned = String::new();
    for character in value
        .trim()
        .chars()
        .filter(|character| !character.is_control() || matches!(character, '\n' | '\t'))
    {
        if cleaned.len() + character.len_utf8() > maximum {
            break;
        }
        cleaned.push(character);
    }
    (!cleaned.is_empty()).then_some(cleaned)
}

fn clean_datetime(value: Option<String>) -> Option<String> {
    let value = value?;
    if value.len() > 64 {
        return None;
    }
    time::OffsetDateTime::parse(&value, &time::format_description::well_known::Rfc3339)
        .ok()?
        .to_offset(time::UtcOffset::UTC)
        .format(&time::format_description::well_known::Rfc3339)
        .ok()
}

fn safe_tracking_url(value: String) -> Option<String> {
    if value.len() > 2048 {
        return None;
    }
    let url = Url::parse(&value).ok()?;
    (url.scheme() == "https"
        && matches!(
            url.host_str(),
            Some("www.aftership.com" | "track.aftership.com")
        )
        && url.username().is_empty()
        && url.password().is_none()
        && url.port().is_none())
    .then(|| url.to_string())
}

#[cfg(test)]
mod tests {
    use std::{
        io::{Read, Write},
        net::{TcpListener, TcpStream},
        sync::{
            atomic::{AtomicBool, Ordering},
            mpsc,
        },
        thread,
    };

    use super::*;
    use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

    const NUMBER: &str = "SPXTEST00000001";
    const ID: &str = "synthetic-tracker-id";

    #[derive(Serialize)]
    struct FixtureEnvelope<T> {
        meta: FixtureMeta,
        data: T,
    }
    #[derive(Serialize)]
    struct FixtureMeta {
        code: u16,
    }
    #[derive(Serialize)]
    struct FixturePagination {
        total: usize,
        next_cursor: &'static str,
        has_next_page: bool,
    }
    #[derive(Serialize)]
    struct FixturePage {
        pagination: FixturePagination,
        trackings: Vec<FixtureTracker>,
    }
    #[derive(Serialize)]
    struct FixtureTracker {
        id: &'static str,
        tracking_number: &'static str,
        slug: &'static str,
        tag: &'static str,
        subtag: &'static str,
        checkpoints: Vec<FixtureCheckpoint>,
        signed_by: Option<&'static str>,
        shipment_delivery_date: Option<&'static str>,
    }
    #[derive(Serialize)]
    struct FixtureCheckpoint {
        tag: &'static str,
        subtag: &'static str,
        checkpoint_time: &'static str,
        created_at: &'static str,
        message: &'static str,
        source: &'static str,
    }
    #[derive(Deserialize)]
    struct RequestedDetection {
        tracking_number: String,
        slug: Vec<String>,
    }
    #[derive(Deserialize)]
    struct RequestedTracking {
        tracking_number: String,
        slug: String,
        courier_connection_id: Option<String>,
    }

    fn pending() -> FixtureTracker {
        FixtureTracker {
            id: ID,
            tracking_number: NUMBER,
            slug: "speedx",
            tag: "Pending",
            subtag: "Pending_001",
            checkpoints: Vec::new(),
            signed_by: None,
            shipment_delivery_date: None,
        }
    }
    fn page(trackings: Vec<FixtureTracker>) -> FixturePage {
        FixturePage {
            pagination: FixturePagination {
                total: trackings.len(),
                next_cursor: "",
                has_next_page: false,
            },
            trackings,
        }
    }
    fn registration(carrier: Carrier) -> Registration {
        Registration {
            tracking_number: NUMBER.into(),
            carrier: Some(carrier),
            started_at: time::OffsetDateTime::now_utc().unix_timestamp(),
            recipient_url: None,
        }
    }

    struct Incoming {
        method: String,
        target: String,
        body: Vec<u8>,
        reply: mpsc::Sender<(u16, String, String)>,
    }
    impl Incoming {
        fn respond(self, status: u16, body: &str) {
            self.reply
                .send((status, body.into(), String::new()))
                .unwrap();
        }
        fn json<T: Serialize>(self, status: u16, data: T) {
            let body = serde_json::to_string(&FixtureEnvelope {
                meta: FixtureMeta { code: status },
                data,
            })
            .unwrap();
            self.respond(status, &body);
        }
        fn limited(self) {
            self.reply
                .send((
                    429,
                    r#"{"meta":{"code":429,"message":"private provider detail"}}"#.into(),
                    "Retry-After: 120\r\nX-Request-Id: 123456781234123412341234567890ab\r\n".into(),
                ))
                .unwrap();
        }
    }
    struct Server {
        base: String,
        incoming: UnboundedReceiver<Incoming>,
        stop: Arc<AtomicBool>,
        thread: Option<thread::JoinHandle<()>>,
    }
    impl Server {
        fn new() -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let base = format!(
                "http://{}/tracking/2026-07/",
                listener.local_addr().unwrap()
            );
            listener.set_nonblocking(true).unwrap();
            let (sender, incoming) = unbounded_channel();
            let stop = Arc::new(AtomicBool::new(false));
            let stopping = stop.clone();
            let thread = thread::spawn(move || {
                while !stopping.load(Ordering::Relaxed) {
                    let (mut stream, _) = match listener.accept() {
                        Ok(connection) => connection,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(2));
                            continue;
                        }
                        Err(_) => return,
                    };
                    let Some((method, target, body)) = read_request(&mut stream) else {
                        continue;
                    };
                    let (reply, receiver) = mpsc::channel();
                    if sender
                        .send(Incoming {
                            method,
                            target,
                            body,
                            reply,
                        })
                        .is_err()
                    {
                        return;
                    }
                    while !stopping.load(Ordering::Relaxed) {
                        match receiver.recv_timeout(Duration::from_millis(20)) {
                            Ok((status, body, headers)) => {
                                let _ = write!(
                                    stream,
                                    "HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\n{headers}Connection: close\r\n\r\n{body}",
                                    body.len()
                                );
                                break;
                            }
                            Err(mpsc::RecvTimeoutError::Timeout) => {}
                            Err(mpsc::RecvTimeoutError::Disconnected) => break,
                        }
                    }
                }
            });
            Self {
                base,
                incoming,
                stop,
                thread: Some(thread),
            }
        }
        fn client(&self) -> AfterShipClient {
            AfterShipClient::for_loopback("synthetic-api-key", &self.base).unwrap()
        }
        async fn request(&mut self, method: &str, path: &str) -> Incoming {
            let request = tokio::time::timeout(Duration::from_secs(5), self.incoming.recv())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(request.method, method);
            assert_eq!(
                request.target.split('?').next().unwrap(),
                format!("/tracking/2026-07/{path}")
            );
            request
        }
        fn no_request(&mut self) {
            assert!(self.incoming.try_recv().is_err());
        }
        async fn empty_lookup(&mut self) {
            self.request("GET", "trackings")
                .await
                .json(200, page(Vec::new()));
        }
        async fn detect(&mut self, slug: &str) {
            let request = self.request("POST", "couriers/detect").await;
            let body: RequestedDetection = serde_json::from_slice(&request.body).unwrap();
            assert_eq!(body.tracking_number, NUMBER);
            assert_eq!(body.slug, [slug]);
            request.respond(200, &format!(r#"{{"meta":{{"code":200}},"data":{{"total":1,"couriers":[{{"slug":"{slug}"}}]}}}}"#));
        }
    }
    impl Drop for Server {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            if let Some(thread) = self.thread.take() {
                thread.join().unwrap();
            }
        }
    }
    fn read_request(stream: &mut TcpStream) -> Option<(String, String, Vec<u8>)> {
        stream.set_nonblocking(false).ok()?;
        stream.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
        let mut header = Vec::new();
        while !header.ends_with(b"\r\n\r\n") {
            let mut byte = [0];
            stream.read_exact(&mut byte).ok()?;
            header.push(byte[0]);
            if header.len() > 16 * 1024 {
                return None;
            }
        }
        let header = String::from_utf8(header).ok()?;
        assert!(
            header
                .to_ascii_lowercase()
                .contains("as-api-key: synthetic-api-key")
        );
        let mut lines = header.lines();
        let mut request = lines.next()?.split_whitespace();
        let method = request.next()?.into();
        let target = request.next()?.into();
        let length = lines
            .filter_map(|line| line.split_once(':'))
            .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
            .and_then(|(_, value)| value.trim().parse::<usize>().ok())
            .unwrap_or_default();
        if length > 16 * 1024 {
            return None;
        }
        let mut body = vec![0; length];
        stream.read_exact(&mut body).ok()?;
        Some((method, target, body))
    }

    #[tokio::test]
    async fn amazon_mcf_uses_captured_amazon_identity_without_a_merchant_connection() {
        let mut server = Server::new();
        let client = server.client();
        let task = tokio::spawn(async move {
            client
                .prepare_registration(registration(Carrier::AmazonMcf))
                .await
        });
        let lookup = server.request("GET", "trackings").await;
        let url = Url::parse(&format!("http://localhost{}", lookup.target)).unwrap();
        assert!(
            url.query_pairs()
                .any(|(key, value)| key == "slug" && value == "amazon")
        );
        lookup.json(200, page(Vec::new()));
        let detection = server.request("POST", "couriers/detect").await;
        let input: RequestedDetection = serde_json::from_slice(&detection.body).unwrap();
        assert_eq!(input.tracking_number, NUMBER);
        assert_eq!(input.slug, ["amazon"]);
        // Sanitized live response shape, with no private tracking identifier.
        detection.respond(200, r#"{"meta":{"code":200},"data":{"total":1,"couriers":[{"slug":"amazon","required_fields":[]}]}}"#);
        let prepared = task.await.unwrap().unwrap();
        server.no_request();
        let task = tokio::spawn(prepared.register());
        let request = server.request("POST", "trackings").await;
        let body: RequestedTracking = serde_json::from_slice(&request.body).unwrap();
        assert_eq!(body.slug, "amazon");
        assert_eq!(body.tracking_number, NUMBER);
        assert_eq!(body.courier_connection_id, None);
        let mut response = pending();
        response.slug = "amazon";
        request.json(201, response);
        let tracker = task.await.unwrap().unwrap();
        assert_eq!(tracker.carrier, Carrier::AmazonMcf);
        assert_eq!(tracker.provider, ProviderId::AfterShip);
        assert!(tracker.carrier_tracking.events.is_empty());
        server.no_request();
    }

    #[tokio::test]
    async fn amazon_mcf_empty_detection_never_falls_back_or_registers() {
        let mut server = Server::new();
        let client = server.client();
        let task = tokio::spawn(async move {
            client
                .prepare_registration(registration(Carrier::AmazonMcf))
                .await
        });
        let lookup = server.request("GET", "trackings").await;
        assert!(lookup.target.contains("slug=amazon"));
        lookup.json(200, page(Vec::new()));
        let detection = server.request("POST", "couriers/detect").await;
        let body: RequestedDetection = serde_json::from_slice(&detection.body).unwrap();
        assert_eq!(body.slug, ["amazon"]);
        detection.respond(
            200,
            r#"{"meta":{"code":200},"data":{"total":0,"couriers":[]}}"#,
        );
        assert_eq!(
            task.await.unwrap().err().unwrap().error_code(),
            "tracking_carrier_mismatch"
        );
        server.no_request();
    }

    #[test]
    fn amazon_products_keep_distinct_provider_routes() {
        assert_eq!(
            ProviderId::for_carrier(Some(Carrier::AmazonMcf)),
            ProviderId::AfterShip
        );
        assert_eq!(
            ProviderId::for_carrier(Some(Carrier::AmazonMws)),
            ProviderId::EasyPost
        );
        assert_eq!(
            ProviderId::for_carrier(Some(Carrier::AmazonShipping)),
            ProviderId::EasyPost
        );
        assert_eq!(ProviderId::for_carrier(None), ProviderId::EasyPost);
    }

    #[tokio::test]
    async fn recognized_unscanned_registration_has_one_write_and_no_fabricated_events() {
        let mut server = Server::new();
        let client = server.client();
        let task = tokio::spawn(async move {
            client
                .prepare_registration(registration(Carrier::SpeedX))
                .await
        });
        server.empty_lookup().await;
        server.detect("speedx").await;
        let prepared = task.await.unwrap().unwrap();
        server.no_request();
        let task = tokio::spawn(prepared.register());
        let request = server.request("POST", "trackings").await;
        let body: RequestedTracking = serde_json::from_slice(&request.body).unwrap();
        assert_eq!(body.tracking_number, NUMBER);
        assert_eq!(body.slug, "speedx");
        assert_eq!(body.courier_connection_id, None);
        request.json(201, pending());
        let tracker = task.await.unwrap().unwrap();
        assert_eq!(tracker.id, ID);
        assert_eq!(tracker.status, Status::Unknown);
        assert!(tracker.carrier_tracking.events.is_empty());
        assert_eq!(tracker.carrier_tracking.delivered_at, None);
        server.no_request();
    }

    #[tokio::test]
    async fn veho_registration_adopts_existing_history_without_a_tracking_import() {
        let mut server = Server::new();
        let client = server.client();
        let task = tokio::spawn(async move {
            client
                .prepare_registration(registration(Carrier::Veho))
                .await
        });
        let mut existing = pending();
        existing.slug = "veho";
        existing.tag = "Delivered";
        existing.subtag = "Delivered_001";
        existing.shipment_delivery_date = Some("2026-01-02T13:00:00-05:00");
        existing.checkpoints = vec![FixtureCheckpoint {
            tag: "Delivered",
            subtag: "Delivered_001",
            checkpoint_time: "2026-01-02T13:00:00-05:00",
            created_at: "2026-01-02T18:01:00Z",
            message: "Package delivered",
            source: "carrier",
        }];
        let lookup = server.request("GET", "trackings").await;
        let url = Url::parse(&format!("http://localhost{}", lookup.target)).unwrap();
        assert!(
            url.query_pairs()
                .any(|(key, value)| key == "slug" && value == "veho")
        );
        lookup.json(200, page(vec![existing]));
        let tracker = task.await.unwrap().unwrap().register().await.unwrap();
        assert_eq!(tracker.provider, ProviderId::AfterShip);
        assert_eq!(tracker.id, ID);
        assert_eq!(tracker.carrier, Carrier::Veho);
        assert_eq!(tracker.status, Status::Delivered);
        assert_eq!(tracker.carrier_tracking.events.len(), 1);
        assert_eq!(
            tracker.carrier_tracking.delivered_at.as_deref(),
            Some("2026-01-02T18:00:00Z")
        );
        server.no_request();
    }

    #[tokio::test]
    async fn new_veho_registration_uses_its_canonical_slug_without_shipper_credentials() {
        let mut server = Server::new();
        let client = server.client();
        let task = tokio::spawn(async move {
            client
                .prepare_registration(registration(Carrier::Veho))
                .await
        });
        server.empty_lookup().await;
        server.detect("veho").await;
        let task = tokio::spawn(task.await.unwrap().unwrap().register());
        let request = server.request("POST", "trackings").await;
        let body: RequestedTracking = serde_json::from_slice(&request.body).unwrap();
        assert_eq!(body.tracking_number, NUMBER);
        assert_eq!(body.slug, "veho");
        assert_eq!(body.courier_connection_id, None);
        let mut tracker = pending();
        tracker.slug = "veho";
        request.json(201, tracker);
        let tracker = task.await.unwrap().unwrap();
        assert_eq!(tracker.provider, ProviderId::AfterShip);
        assert_eq!(tracker.carrier, Carrier::Veho);
        assert!(tracker.carrier_tracking.events.is_empty());
        server.no_request();
    }

    #[tokio::test]
    async fn lost_registration_response_is_adopted_after_restart_without_another_write() {
        for timeout_after_registration in [false, true] {
            let mut server = Server::new();
            let mut client = server.client();
            if timeout_after_registration {
                client.client = Client::builder()
                    .timeout(Duration::from_millis(150))
                    .retry(reqwest::retry::never())
                    .build()
                    .unwrap();
            }
            let task = tokio::spawn(async move {
                client
                    .prepare_registration(registration(Carrier::SpeedX))
                    .await
            });
            server.empty_lookup().await;
            server.detect("speedx").await;
            let task = tokio::spawn(task.await.unwrap().unwrap().register());
            let pending_response = server.request("POST", "trackings").await;
            if timeout_after_registration {
                assert_eq!(
                    task.await.unwrap().unwrap_err().error_code(),
                    "provider_timeout"
                );
                drop(pending_response);
            } else {
                drop(pending_response);
                assert_eq!(
                    task.await.unwrap().unwrap_err().error_code(),
                    "provider_connection"
                );
            }
            let restarted = server.client();
            let task = tokio::spawn(async move {
                restarted
                    .prepare_registration(registration(Carrier::SpeedX))
                    .await
            });
            server
                .request("GET", "trackings")
                .await
                .json(200, page(vec![pending()]));
            server.detect("speedx").await;
            let tracker = task.await.unwrap().unwrap().register().await.unwrap();
            assert_eq!(tracker.id, ID);
            assert_eq!(tracker.status, Status::Unknown);
            server.no_request();
        }
    }

    #[tokio::test]
    async fn dropping_a_prepared_registration_releases_the_slot_without_a_write() {
        let mut server = Server::new();
        let client = server.client();
        let preparing = client.clone();
        let task = tokio::spawn(async move {
            preparing
                .prepare_registration(registration(Carrier::SpeedX))
                .await
        });
        server.empty_lookup().await;
        server.detect("speedx").await;
        drop(task.await.unwrap().unwrap());
        server.no_request();
        let task = tokio::spawn(async move { client.check_connection().await });
        server.request("GET", "trackings").await.respond(200, r#"{"meta":{"code":200},"data":{"trackings":[{"id":"another-carrier-record","slug":"amazon"}]}}"#);
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn format_rejection_never_creates_a_tracking_record() {
        let mut server = Server::new();
        let client = server.client();
        let task = tokio::spawn(async move {
            client
                .prepare_registration(registration(Carrier::SpeedX))
                .await
        });
        server.empty_lookup().await;
        server.request("POST", "couriers/detect").await.respond(
            200,
            r#"{"meta":{"code":200},"data":{"total":0,"couriers":[]}}"#,
        );
        let error = task.await.unwrap().err().unwrap();
        assert_eq!(error.error_code(), "tracking_carrier_mismatch");
        assert!(error.rejects_registration());
        server.no_request();
    }

    #[tokio::test]
    async fn existing_sway_configuration_error_is_actionable_and_never_recreated() {
        let mut server = Server::new();
        let client = server.client();
        let task = tokio::spawn(async move {
            client
                .prepare_registration(registration(Carrier::Sway))
                .await
        });
        let mut tracker = pending();
        tracker.slug = "sway-webhook";
        tracker.subtag = "Pending_002";
        let response = serde_json::to_string(&FixtureEnvelope {
            meta: FixtureMeta { code: 200 },
            data: page(vec![tracker]),
        })
        .unwrap();
        server
            .request("GET", "trackings")
            .await
            .reply
            .send((
                200,
                response,
                "X-Request-Id: 123456781234123412341234567890ab\r\n".into(),
            ))
            .unwrap();
        let error = task.await.unwrap().err().unwrap();
        assert!(error.requires_configuration());
        assert!(!error.rejects_registration());
        assert_eq!(error.provider_error_code(), Some("Pending_002"));
        assert_eq!(error.provider_status(), None);
        assert_eq!(
            error.provider_request_id,
            Some(Uuid::parse_str("123456781234123412341234567890ab").unwrap())
        );
        assert!(!error.to_string().contains(NUMBER));
        server.no_request();
    }

    #[tokio::test]
    async fn missing_sway_connection_is_caught_before_any_paid_tracking_import() {
        let mut server = Server::new();
        let client = server.client();
        let task = tokio::spawn(async move {
            client
                .prepare_registration(registration(Carrier::Sway))
                .await
        });
        server.empty_lookup().await;
        server.detect("sway-webhook").await;
        server.request("GET", "courier-connections").await.respond(200, r#"{"meta":{"code":200},"data":{"pagination":{"total":0,"next_cursor":"","has_next_page":false},"courier_connections":[]}}"#);
        let error = task.await.unwrap().err().unwrap();
        assert!(error.requires_configuration());
        assert!(!error.rejects_registration());
        server.no_request();
    }

    #[tokio::test]
    async fn configured_sway_uses_its_verified_connection_and_exact_slug() {
        let mut server = Server::new();
        let client = server.client();
        let task = tokio::spawn(async move {
            client
                .prepare_registration(registration(Carrier::Sway))
                .await
        });
        server.empty_lookup().await;
        server.detect("sway-webhook").await;
        server.request("GET", "courier-connections").await.respond(200, r#"{"meta":{"code":200},"data":{"pagination":{"total":1,"next_cursor":"","has_next_page":false},"courier_connections":[{"id":"synthetic-connection","courier_slug":"sway-webhook"}]}}"#);
        let task = tokio::spawn(task.await.unwrap().unwrap().register());
        let request = server.request("POST", "trackings").await;
        let body: RequestedTracking = serde_json::from_slice(&request.body).unwrap();
        assert_eq!(body.slug, "sway-webhook");
        assert_eq!(
            body.courier_connection_id.as_deref(),
            Some("synthetic-connection")
        );
        let mut tracker = pending();
        tracker.slug = "sway-webhook";
        tracker.tag = "InfoReceived";
        tracker.subtag = "InfoReceived_001";
        request.json(201, tracker);
        assert_eq!(task.await.unwrap().unwrap().carrier, Carrier::Sway);
        server.no_request();
    }

    #[tokio::test]
    async fn carrier_scan_times_survive_import_and_manual_completion_is_not_a_scan() {
        let mut server = Server::new();
        let client = server.client();
        let task = tokio::spawn(async move { client.retrieve(ID, NUMBER).await });
        let mut tracker = pending();
        tracker.tag = "Delivered";
        tracker.subtag = "Delivered_001";
        tracker.shipment_delivery_date = Some("2026-01-02T13:00:00-05:00");
        tracker.checkpoints = vec![
            FixtureCheckpoint {
                tag: "InTransit",
                subtag: "InTransit_001",
                checkpoint_time: "2026-01-01T10:00:00-05:00",
                created_at: "2026-02-01T00:00:00Z",
                message: "Carrier accepted package",
                source: "carrier",
            },
            FixtureCheckpoint {
                tag: "OutForDelivery",
                subtag: "OutForDelivery_001",
                checkpoint_time: "2026-01-02T08:00:00-05:00",
                created_at: "2026-02-01T00:00:00Z",
                message: "Out for delivery",
                source: "carrier",
            },
            FixtureCheckpoint {
                tag: "Delivered",
                subtag: "Delivered_003",
                checkpoint_time: "2026-01-02T12:00:00-05:00",
                created_at: "2026-02-01T00:00:00Z",
                message: "Package delivered",
                source: "carrier",
            },
            FixtureCheckpoint {
                tag: "Delivered",
                subtag: "Delivered_001",
                checkpoint_time: "2026-02-01T12:00:00Z",
                created_at: "2026-02-01T12:00:00Z",
                message: "Manually completed",
                source: "user",
            },
        ];
        server
            .request("GET", &format!("trackings/{ID}"))
            .await
            .json(200, tracker);
        let tracker = task.await.unwrap().unwrap();
        assert_eq!(tracker.status, Status::Delivered);
        assert_eq!(tracker.carrier_tracking.events.len(), 3);
        assert_eq!(
            tracker.carrier_tracking.delivered_at.as_deref(),
            Some("2026-01-02T17:00:00Z")
        );
        assert_eq!(tracker.carrier_tracking.signed_by, None);
        assert_eq!(tracker.carrier_tracking.estimated_delivery_at, None);
        assert_eq!(
            tracker.carrier_tracking.events[0].datetime.as_deref(),
            Some("2026-01-01T15:00:00Z")
        );
    }

    #[tokio::test]
    async fn complete_paginated_lookup_is_required_before_adopting_an_existing_record() {
        let mut server = Server::new();
        let client = server.client();
        let task = tokio::spawn(async move {
            client
                .reconcile_registration(&registration(Carrier::SpeedX))
                .await
        });
        let mut first = page(vec![pending()]);
        first.pagination.has_next_page = true;
        first.pagination.next_cursor = "second";
        server.request("GET", "trackings").await.json(200, first);
        let request = server.request("GET", "trackings").await;
        assert!(request.target.contains("cursor=second"));
        let mut last = page(Vec::new());
        last.pagination.total = 1;
        request.json(200, last);
        server.detect("speedx").await;
        assert_eq!(task.await.unwrap().unwrap().unwrap().id, ID);
        server.no_request();
    }

    #[tokio::test]
    async fn malformed_incomplete_or_ambiguous_lookup_never_permits_registration() {
        for case in 0..5 {
            let mut server = Server::new();
            let client = server.client();
            let task = tokio::spawn(async move {
                client
                    .prepare_registration(registration(Carrier::SpeedX))
                    .await
            });
            let response = match case {
                0 => page(vec![pending(), pending()]),
                1 => {
                    let mut page = page(Vec::new());
                    page.pagination.has_next_page = true;
                    page
                }
                2 => {
                    let mut page = page(Vec::new());
                    page.pagination.total = 1;
                    page
                }
                3 => {
                    let mut tracker = pending();
                    tracker.tracking_number = "DIFFERENT-SYNTHETIC-NUMBER";
                    page(vec![tracker])
                }
                _ => {
                    let mut tracker = pending();
                    tracker.slug = "sway-webhook";
                    page(vec![tracker])
                }
            };
            server.request("GET", "trackings").await.json(200, response);
            assert!(task.await.unwrap().is_err());
            server.no_request();
        }
    }

    #[tokio::test]
    async fn rate_limit_cooldown_is_shared_and_safe_request_ids_are_retained() {
        let mut server = Server::new();
        let client = server.client();
        let first = client.clone();
        let task = tokio::spawn(async move { first.check_connection().await });
        server.request("GET", "trackings").await.limited();
        let error = task.await.unwrap().unwrap_err();
        assert_eq!(error.retry_after(), Some(120));
        assert_eq!(
            error.provider_request_id,
            Some(Uuid::parse_str("123456781234123412341234567890ab").unwrap())
        );
        assert!(!format!("{error:?} {error}").contains("private provider detail"));
        assert!(client.blocked());
        assert_eq!(
            client.check_connection().await.unwrap_err().error_code(),
            "provider_throttled"
        );
        server.no_request();
    }

    #[tokio::test]
    async fn provider_outage_cannot_turn_a_native_invalid_code_into_final_rejection() {
        for status in [400, 503] {
            let mut server = Server::new();
            let client = server.client();
            let task = tokio::spawn(async move {
                client
                    .prepare_registration(registration(Carrier::SpeedX))
                    .await
            });
            server.request("GET", "trackings").await.respond(
                status,
                r#"{"meta":{"code":4005,"message":"private provider detail"}}"#,
            );
            let error = task.await.unwrap().err().unwrap();
            assert_eq!(error.provider_status(), Some(status));
            assert_eq!(error.provider_error_code(), Some("4005"));
            assert_eq!(error.rejects_registration(), status == 400);
            assert!(!error.requires_configuration());
            assert!(!format!("{error:?} {error}").contains("private provider detail"));
            server.no_request();
        }
    }

    #[tokio::test]
    async fn expired_registration_cannot_create_again_after_provider_retention() {
        let mut server = Server::new();
        let client = server.client();
        let mut registration = registration(Carrier::SpeedX);
        registration.started_at -= RETENTION_SECONDS;
        let task = tokio::spawn(async move { client.prepare_registration(registration).await });
        server.empty_lookup().await;
        let error = task.await.unwrap().err().unwrap();
        assert_eq!(error.error_code(), "registration_expired");
        server.no_request();
    }
}
