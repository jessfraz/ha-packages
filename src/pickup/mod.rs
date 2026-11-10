//! Direct carrier pickups. Clients own wire contracts; the service owns consent,
//! durable dispatch and reconciliation. A tracking credential is not permission
//! to dispatch a driver.

use std::{future::Future, pin::Pin};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub(crate) mod api;
pub mod fedex;
mod http;
pub mod models;
pub mod service;
mod store;
pub mod ups;
pub mod usps;

use crate::pickup::models::{Confirmation, Offer, ResolvedPlan};

#[derive(Clone, Copy, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Environment {
    Production,
    #[default]
    Sandbox,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Credentials {
    pub client_id: String,
    pub client_secret: String,
    #[serde(default)]
    pub environment: Environment,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AccountCredentials {
    pub client_id: String,
    pub client_secret: String,
    pub account_number: String,
    #[serde(default)]
    pub environment: Environment,
}

#[derive(Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    pub usps: Option<Credentials>,
    pub ups: Option<AccountCredentials>,
    pub fedex: Option<AccountCredentials>,
}

pub type Result<T> = std::result::Result<T, Error>;
pub type PickupFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T>> + Send + 'a>>;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Lookup {
    Confirmation,
    Pending,
    Unsupported,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
pub struct Capabilities {
    pub lookup: Lookup,
    pub free_normal_route: bool,
    pub custom_window: bool,
    pub services: Vec<models::PickupService>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    InvalidInput,
    NotConfigured,
    Unauthorized,
    Unavailable,
    RateLimited,
    Ineligible,
    NotFound,
    Conflict,
    Unsupported,
    UnknownOutcome,
    StorageUnavailable,
    FeeConsentRequired,
    ParcelDetailsRequired,
    WatchNotReady,
    LocationRequired,
    LocationChanged,
    ConfigurationChanged,
    Ambiguous,
}

/// Fixed diagnostics only. Provider errors can contain addresses, account
/// numbers, instructions and credentials and never cross this boundary.
#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, thiserror::Error)]
#[error("{code}")]
pub struct Error {
    pub code: ErrorCode,
    pub retry_after_seconds: Option<u64>,
}

impl std::fmt::Display for ErrorCode {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::InvalidInput => "Pickup details are missing or invalid",
            Self::NotConfigured => "This carrier's pickup account is not configured",
            Self::Unauthorized => "The carrier has not authorized this pickup account",
            Self::Unavailable => "The carrier is unavailable; existing pickup records are unchanged",
            Self::RateLimited => "The carrier has paused requests; wait before trying again",
            Self::Ineligible => "No eligible pickup was available for these details",
            Self::NotFound => "The pickup or saved location was not found",
            Self::Conflict => "Pickup details changed or this label already has an active pickup",
            Self::Unsupported => "This carrier does not support this pickup operation",
            Self::UnknownOutcome => "The carrier outcome is unknown; do not schedule another pickup before reconciliation",
            Self::StorageUnavailable => "Pickup storage is unavailable",
            Self::FeeConsentRequired => "Pass --accept-fee to authorize the pickup charge billed to the configured carrier account",
            Self::ParcelDetailsRequired => "Pickup service and weight are required; save the label's details or provide --service and --weight-lb",
            Self::WatchNotReady => "Tracking must be verified and actively watched before a pickup can be scheduled",
            Self::LocationChanged => "The pickup address changed. Review it again before scheduling",
            Self::ConfigurationChanged => "The carrier configuration changed. Review the pickup again before scheduling",
            Self::LocationRequired => "Configure and confirm this deployment's default pickup location first",
            Self::Ambiguous => "More than one match was found; provide an explicit carrier or confirmation",
        })
    }
}

impl Error {
    pub fn new(code: ErrorCode) -> Self {
        Self {
            code,
            retry_after_seconds: None,
        }
    }
}

impl From<Error> for dropshot::HttpError {
    fn from(error: Error) -> Self {
        let code = match error.code {
            ErrorCode::InvalidInput => "pickup_invalid_input",
            ErrorCode::NotConfigured => "pickup_not_configured",
            ErrorCode::Unauthorized => "pickup_unauthorized",
            ErrorCode::Unavailable => "pickup_unavailable",
            ErrorCode::RateLimited => "pickup_rate_limited",
            ErrorCode::Ineligible => "pickup_ineligible",
            ErrorCode::NotFound => "pickup_not_found",
            ErrorCode::Conflict => "pickup_conflict",
            ErrorCode::Unsupported => "pickup_unsupported",
            ErrorCode::UnknownOutcome => "pickup_unknown_outcome",
            ErrorCode::StorageUnavailable => "pickup_storage_unavailable",
            ErrorCode::FeeConsentRequired => "pickup_fee_consent_required",
            ErrorCode::ParcelDetailsRequired => "pickup_parcel_details_required",
            ErrorCode::WatchNotReady => "pickup_watch_not_ready",
            ErrorCode::LocationRequired => "pickup_location_required",
            ErrorCode::LocationChanged => "pickup_location_changed",
            ErrorCode::ConfigurationChanged => "pickup_configuration_changed",
            ErrorCode::Ambiguous => "pickup_ambiguous",
        };
        let mut response = match error.code {
            ErrorCode::Unavailable | ErrorCode::NotConfigured | ErrorCode::StorageUnavailable => {
                let mut response = Self::for_unavail(Some(code.into()), error.to_string());
                response.external_message = error.to_string();
                response
            }
            _ => Self::for_client_error(
                Some(code.into()),
                match error.code {
                    ErrorCode::Unauthorized => dropshot::ClientErrorStatusCode::FORBIDDEN,
                    ErrorCode::NotFound => dropshot::ClientErrorStatusCode::NOT_FOUND,
                    ErrorCode::LocationChanged | ErrorCode::ConfigurationChanged => {
                        dropshot::ClientErrorStatusCode::PRECONDITION_FAILED
                    }
                    ErrorCode::Conflict
                    | ErrorCode::UnknownOutcome
                    | ErrorCode::WatchNotReady
                    | ErrorCode::Ambiguous => dropshot::ClientErrorStatusCode::CONFLICT,
                    ErrorCode::RateLimited => dropshot::ClientErrorStatusCode::TOO_MANY_REQUESTS,
                    _ => dropshot::ClientErrorStatusCode::BAD_REQUEST,
                },
                error.to_string(),
            ),
        };
        if let Some(delay) = error.retry_after_seconds
            && let Ok(header) = ::http::HeaderValue::from_str(&delay.to_string())
        {
            response
                .headers_mut()
                .insert(::http::header::RETRY_AFTER, header);
        }
        response
    }
}

impl From<sqlx::Error> for Error {
    fn from(_: sqlx::Error) -> Self {
        Self::new(ErrorCode::StorageUnavailable)
    }
}

impl From<serde_json::Error> for Error {
    fn from(_: serde_json::Error) -> Self {
        Self::new(ErrorCode::StorageUnavailable)
    }
}

/// Preview is read-only availability plus charge information where supported.
/// Not every carrier can quote a fee or retrieve a previously created pickup.
pub trait Pickup: Send + Sync {
    fn carrier(&self) -> models::PickupCarrier;
    fn capabilities(&self) -> Capabilities;
    /// Binds saved work to the configured environment and billing account.
    /// Only the one-way digest is persisted, never credentials or account IDs.
    fn fingerprint(&self) -> [u8; 32];
    fn preview<'a>(&'a self, plan: &'a ResolvedPlan) -> PickupFuture<'a, Offer>;
    fn schedule<'a>(&'a self, plan: &'a ResolvedPlan) -> PickupFuture<'a, Confirmation>;
    fn lookup<'a>(
        &'a self,
        plan: &'a ResolvedPlan,
        confirmation: &'a Confirmation,
    ) -> PickupFuture<'a, Confirmation>;
    fn cancel<'a>(
        &'a self,
        plan: &'a ResolvedPlan,
        confirmation: &'a Confirmation,
    ) -> PickupFuture<'a, ()>;
}

#[cfg(test)]
mod tests;
