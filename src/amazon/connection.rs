//! Fixed-vocabulary connection diagnostics. Provider bodies and credentials
//! never enter this type, the API error response, or operational logs.

use sha2::{Digest, Sha256};
use std::fmt;

use crate::{
    amazon::provider_error,
    shipping_api::error::{Error, ErrorKind},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConnectionStage {
    Callback,
    Flow,
    Registration,
    AccessToken,
    Cookies,
    Orders,
    Account,
    Credentials,
}

impl ConnectionStage {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Callback => "callback",
            Self::Flow => "flow",
            Self::Registration => "registration",
            Self::AccessToken => "access_token",
            Self::Cookies => "cookies",
            Self::Orders => "orders",
            Self::Account => "account",
            Self::Credentials => "credentials",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PageKind {
    Orders,
    Item,
    Tracking,
}
impl PageKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Orders => "orders_listing",
            Self::Item => "item_details",
            Self::Tracking => "tracking_page",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConnectionReason {
    InvalidInput,
    ExpiredFlow,
    AccountIdentityMissing,
    AccountMismatch,
    SessionChanged,
    UpstreamRejected,
    UnexpectedResponse,
    Timeout,
    Connection,
    RateLimited,
    UpstreamHttp,
    Persistence,
    Configuration,
    UnsupportedOrdersLayout,
    ChallengePage,
    InProgress,
}
impl ConnectionReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::InvalidInput => "invalid_input",
            Self::ExpiredFlow => "expired_flow",
            Self::AccountIdentityMissing => "account_identity_missing",
            Self::AccountMismatch => "account_mismatch",
            Self::SessionChanged => "session_changed",
            Self::UpstreamRejected => "upstream_rejected",
            Self::UnexpectedResponse => "unexpected_response",
            Self::Timeout => "timeout",
            Self::Connection => "connection",
            Self::RateLimited => "rate_limited",
            Self::UpstreamHttp => "upstream_http",
            Self::Persistence => "persistence",
            Self::Configuration => "configuration",
            Self::UnsupportedOrdersLayout => "unsupported_orders_layout",
            Self::ChallengePage => "challenge_page",
            Self::InProgress => "in_progress",
        }
    }
}

#[derive(Debug)]
pub struct ConnectionError {
    pub stage: ConnectionStage,
    pub reason: ConnectionReason,
    pub upstream_status: Option<u16>,
    source: Error,
    pub response_shape: Option<&'static str>,
    pub page_kind: Option<PageKind>,
    pub page_fingerprint: Option<Box<str>>,
}
impl ConnectionError {
    pub(crate) fn new(stage: ConnectionStage, source: Error) -> Self {
        let reason = match source.kind {
            ErrorKind::AuthenticationRequired => ConnectionReason::UpstreamRejected,
            ErrorKind::ProviderChallenge { .. } => ConnectionReason::ChallengePage,
            ErrorKind::Timeout => ConnectionReason::Timeout,
            ErrorKind::Connection => ConnectionReason::Connection,
            ErrorKind::Throttled { .. } | ErrorKind::Http { status: 429, .. } => {
                ConnectionReason::RateLimited
            }
            ErrorKind::Http { .. } => ConnectionReason::UpstreamHttp,
            ErrorKind::Configuration => ConnectionReason::Configuration,
            _ => ConnectionReason::UnexpectedResponse,
        };
        let upstream_status = source.provider_status();
        Self {
            stage,
            reason,
            upstream_status,
            source,
            response_shape: None,
            page_kind: None,
            page_fingerprint: None,
        }
    }
    pub(crate) fn with_status(stage: ConnectionStage, source: Error, status: u16) -> Self {
        Self::new(stage, source).with_upstream_status(status)
    }
    pub(crate) fn failure(stage: ConnectionStage, reason: ConnectionReason) -> Self {
        Self {
            stage,
            reason,
            upstream_status: None,
            source: provider_error(ErrorKind::InvalidResponse),
            response_shape: None,
            page_kind: None,
            page_fingerprint: None,
        }
    }
    pub(crate) fn with_upstream_status(mut self, status: u16) -> Self {
        self.upstream_status = (100..=599).contains(&status).then_some(status);
        self
    }
    pub fn code(&self) -> String {
        format!(
            "amazon_connect_{}_{}",
            self.stage.as_str(),
            self.reason.as_str()
        )
    }
    pub(crate) fn provider_error(&self) -> &Error {
        &self.source
    }
    pub(crate) fn with_shape(mut self, shape: &'static str) -> Self {
        self.response_shape = Some(shape);
        self
    }
    pub(crate) fn with_page_kind(mut self, kind: PageKind) -> Self {
        self.page_kind = Some(kind);
        self
    }
    pub(crate) fn with_path(mut self, path: &str) -> Self {
        self.page_fingerprint = Some(
            Sha256::digest(path.as_bytes())
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
                .into_boxed_str(),
        );
        self
    }
    pub fn retry_after(&self) -> Option<i64> {
        self.source.retry_after()
    }
    pub fn into_provider(self) -> Error {
        self.source
    }
}
impl From<ConnectionError> for Error {
    fn from(error: ConnectionError) -> Self {
        error.into_provider()
    }
}
impl fmt::Display for ConnectionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Amazon connection failed during {} ({})",
            self.stage.as_str(),
            self.reason.as_str()
        )?;
        if let Some(status) = self.upstream_status {
            write!(f, "; upstream HTTP {status}")?;
        }
        Ok(())
    }
}
impl std::error::Error for ConnectionError {}
