use std::fmt;

use crate::shipping_api::types::ProviderId;

/// Safe, normalized failures. Never retain provider free text, request URLs,
/// credentials or response bodies. Native codes must be static adapter values.
pub struct Error {
    pub provider: ProviderId,
    pub kind: ErrorKind,
    pub provider_request_id: Option<uuid::Uuid>,
    retry_after_seconds: Option<i64>,
}

#[derive(Debug)]
pub enum ErrorKind {
    AuthenticationRequired,
    ProviderChallenge {
        status: Option<u16>,
        retry_after: Option<i64>,
    },
    Configuration,
    Timeout,
    LocalQueueTimeout,
    LocalQueueFull,
    Connection,
    Http {
        status: u16,
        retry_after: Option<i64>,
    },
    Throttled {
        retry_after: i64,
    },
    Rejected(Rejection),
    ProviderRejected {
        rejection: Rejection,
        code: &'static str,
        status: u16,
        retry_after: Option<i64>,
    },
    ProviderStateRejected {
        rejection: Rejection,
        code: &'static str,
    },
    RegistrationAmbiguous,
    RegistrationExpired,
    ResponseTooLarge,
    InvalidResponse,
    InvalidConnectionResponse,
    RecipientStatusUnavailable,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rejection {
    NoCarrierForCode,
    CarrierCodeMismatch,
    MultipleCarriersForCode,
    InvalidParams,
    InvalidTestCode,
    NoCarrierAccount,
    CarrierCredentialsMissing,
    RecipientLinkRequired,
    InvalidRecipientLink,
    RecipientLinkMismatch,
}

impl Error {
    pub fn new(provider: ProviderId, kind: ErrorKind) -> Self {
        Self {
            provider,
            kind,
            provider_request_id: None,
            retry_after_seconds: None,
        }
    }

    pub fn error_code(&self) -> &'static str {
        match &self.kind {
            ErrorKind::ProviderRejected { status, .. } if !matches!(status, 400 | 404 | 422) => {
                http_error_code(*status)
            }
            ErrorKind::Rejected(rejection)
            | ErrorKind::ProviderRejected { rejection, .. }
            | ErrorKind::ProviderStateRejected { rejection, .. } => rejection.error_code(),
            ErrorKind::RegistrationAmbiguous => "registration_ambiguous",
            ErrorKind::RegistrationExpired => "registration_expired",
            ErrorKind::AuthenticationRequired => "provider_authentication_required",
            ErrorKind::ProviderChallenge { .. } => "provider_challenge_required",
            ErrorKind::Configuration => "provider_configuration",
            ErrorKind::Timeout => "provider_timeout",
            ErrorKind::LocalQueueTimeout => "local_provider_queue_timeout",
            ErrorKind::LocalQueueFull => "local_provider_queue_full",
            ErrorKind::Connection => "provider_connection",
            ErrorKind::Http {
                status: 401 | 403, ..
            } if self.provider == ProviderId::DoorDash => "recipient_page_unavailable",
            ErrorKind::Http { status, .. } => http_error_code(*status),
            ErrorKind::Throttled { .. } => "provider_throttled",
            ErrorKind::ResponseTooLarge => "provider_response_too_large",
            ErrorKind::InvalidResponse | ErrorKind::InvalidConnectionResponse => {
                "provider_invalid_response"
            }
            ErrorKind::RecipientStatusUnavailable => "recipient_page_unavailable",
        }
    }

    pub fn provider_status(&self) -> Option<u16> {
        match self.kind {
            ErrorKind::ProviderChallenge { status, .. } => status,
            ErrorKind::Http { status, .. } | ErrorKind::ProviderRejected { status, .. } => {
                Some(status)
            }
            _ => None,
        }
    }

    pub fn provider_error_code(&self) -> Option<&'static str> {
        match self.kind {
            ErrorKind::ProviderRejected { code, .. }
            | ErrorKind::ProviderStateRejected { code, .. } => Some(code),
            _ => None,
        }
    }

    pub fn retry_after(&self) -> Option<i64> {
        let value = match self.kind {
            ErrorKind::ProviderChallenge { retry_after, .. } => retry_after,
            ErrorKind::Http { retry_after, .. }
            | ErrorKind::ProviderRejected { retry_after, .. } => retry_after,
            ErrorKind::Throttled { retry_after } => Some(retry_after),
            _ => None,
        };
        self.retry_after_seconds.or(value)
    }

    pub(crate) fn set_retry_after(&mut self, value: i64) {
        let value = value.max(0);
        self.retry_after_seconds = Some(value);
        match &mut self.kind {
            ErrorKind::Http { retry_after, .. }
            | ErrorKind::ProviderChallenge { retry_after, .. }
            | ErrorKind::ProviderRejected { retry_after, .. } => *retry_after = Some(value),
            ErrorKind::Throttled { retry_after } => *retry_after = value,
            _ => {}
        }
    }

    pub fn rejects_registration(&self) -> bool {
        if matches!(self.kind, ErrorKind::ProviderRejected { status, .. } if !matches!(status,400|404|422))
        {
            return false;
        }
        matches!(
            self.kind,
            ErrorKind::Rejected(
                Rejection::NoCarrierForCode
                    | Rejection::CarrierCodeMismatch
                    | Rejection::MultipleCarriersForCode
                    | Rejection::InvalidParams
                    | Rejection::RecipientLinkRequired
                    | Rejection::InvalidRecipientLink
                    | Rejection::RecipientLinkMismatch
            ) | ErrorKind::ProviderRejected {
                rejection: Rejection::NoCarrierForCode
                    | Rejection::CarrierCodeMismatch
                    | Rejection::MultipleCarriersForCode
                    | Rejection::InvalidParams,
                ..
            } | ErrorKind::ProviderStateRejected {
                rejection: Rejection::NoCarrierForCode
                    | Rejection::CarrierCodeMismatch
                    | Rejection::MultipleCarriersForCode
                    | Rejection::InvalidParams
                    | Rejection::RecipientLinkRequired
                    | Rejection::InvalidRecipientLink
                    | Rejection::RecipientLinkMismatch,
                ..
            } | ErrorKind::RegistrationAmbiguous
                | ErrorKind::RegistrationExpired
        )
    }

    pub fn requires_configuration(&self) -> bool {
        configuration_error_code(self.error_code())
    }
}

pub fn configuration_error_code(code: &str) -> bool {
    matches!(
        code,
        "provider_authentication_required"
            | "tracking_provider_credentials"
            | "tracking_carrier_account_required"
            | "tracking_carrier_credentials_required"
            | "tracking_test_key"
    )
}

fn http_error_code(status: u16) -> &'static str {
    match status {
        401 | 403 => "tracking_provider_credentials",
        429 => "tracking_verification_rate_limited",
        _ => "provider_http",
    }
}

impl Rejection {
    pub fn error_code(self) -> &'static str {
        match self {
            Self::NoCarrierForCode => "tracking_unsupported",
            Self::CarrierCodeMismatch => "tracking_carrier_mismatch",
            Self::MultipleCarriersForCode => "tracking_carrier_ambiguous",
            Self::InvalidParams => "tracking_invalid",
            Self::InvalidTestCode => "tracking_test_key",
            Self::NoCarrierAccount => "tracking_carrier_account_required",
            Self::CarrierCredentialsMissing => "tracking_carrier_credentials_required",
            Self::RecipientLinkRequired => "tracking_recipient_url_required",
            Self::InvalidRecipientLink => "tracking_recipient_url_invalid",
            Self::RecipientLinkMismatch => "tracking_recipient_url_mismatch",
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let provider = self.provider.name();
        match &self.kind {
            ErrorKind::AuthenticationRequired => write!(f, "{provider} requires sign-in again"),
            ErrorKind::ProviderChallenge { .. } => write!(f, "{provider} requires website verification; the saved sign-in is retained"),
            ErrorKind::Configuration => write!(f, "invalid {provider} client configuration"),
            ErrorKind::Timeout => write!(f, "{provider} request timed out"),
            ErrorKind::LocalQueueTimeout => write!(f, "Local {provider} request queue deadline expired before sending"),
            ErrorKind::LocalQueueFull => write!(f, "Local {provider} request queue is full; no request was sent"),
            ErrorKind::Connection => write!(f, "{provider} connection failed"),
            ErrorKind::Http { status, .. } => write!(f, "{provider} returned HTTP {status}"),
            ErrorKind::Throttled { retry_after } => write!(
                f,
                "{provider} requests are paused; retry in {retry_after} seconds"
            ),
            ErrorKind::RegistrationAmbiguous => write!(
                f,
                "{provider} returned multiple possible trackers for this registration. Select the correct carrier before retrying; no new tracker was created."
            ),
            ErrorKind::RegistrationExpired => write!(
                f,
                "The original {provider} registration could not be confirmed within its duplicate-protection window. Review the tracker in {provider} before registering it again; no new tracker was created."
            ),
            ErrorKind::ResponseTooLarge => write!(f, "{provider} response exceeded the size limit"),
            ErrorKind::InvalidResponse => write!(f, "{provider} returned an invalid tracker"),
            ErrorKind::InvalidConnectionResponse => write!(
                f,
                "{provider} returned an invalid connection-check response"
            ),
            ErrorKind::RecipientStatusUnavailable => f.write_str("DoorDash's recipient page did not provide a recognized current tracking status. No shipment was verified. Retry later; if this persists, the tracking page format may have changed."),
            ErrorKind::ProviderRejected { status, .. } if !matches!(status, 400 | 404 | 422) => {
                write!(f, "{provider} returned HTTP {status}")
            }
            ErrorKind::Rejected(rejection) | ErrorKind::ProviderRejected { rejection, .. }
            | ErrorKind::ProviderStateRejected { rejection, .. } => {
                format_rejection(*rejection, provider, f)
            }
        }
    }
}

impl std::error::Error for Error {}

impl fmt::Debug for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ShippingError")
            .field("provider", &self.provider)
            .field("error_code", &self.error_code())
            .field("status", &self.provider_status())
            .field("retry_after", &self.retry_after())
            .field("provider_request_id", &self.provider_request_id)
            .finish()
    }
}

fn format_rejection(
    rejection: Rejection,
    provider: &str,
    f: &mut fmt::Formatter<'_>,
) -> fmt::Result {
    match rejection {
                Rejection::NoCarrierForCode => write!(f, "{provider} could not match this tracking number to an available carrier. Select the carrier shown on the shipping notice and confirm its carrier account is configured in {provider}. This does not mean the tracking number is invalid."),
                Rejection::CarrierCodeMismatch => f.write_str("The tracking number does not match the selected carrier. Check the number and carrier."),
                Rejection::MultipleCarriersForCode => f.write_str("The tracking number matches multiple carriers. Specify its carrier."),
                Rejection::InvalidParams => write!(f, "{provider} could not accept this tracking number and carrier. Check both values."),
                Rejection::InvalidTestCode => write!(f, "This {provider} test key accepts only {provider} test tracking numbers. Use a production key for real shipments."),
                Rejection::NoCarrierAccount => write!(f, "The {provider} account needs a carrier account for this carrier. Configure it in {provider} and try again."),
                Rejection::CarrierCredentialsMissing => write!(f, "{provider} has no credentials for this carrier. Configure this carrier in {provider}, then retry. This does not mean the tracking number is invalid."),
                Rejection::RecipientLinkRequired => f.write_str("DoorDash requires its original recipient tracking link in tracking.recipient_url. Copy the link from the merchant's shipping email."),
                Rejection::InvalidRecipientLink => f.write_str("Use the original HTTPS DoorDash recipient link: https://www.doordash.com/orders/drive?urlCode=... . Other hosts, paths, credentials and duplicate link codes are not accepted."),
                Rejection::RecipientLinkMismatch => f.write_str("The DoorDash recipient page belongs to a different tracking number. Check tracking.number and tracking.recipient_url; no shipment was verified."),
    }
}
