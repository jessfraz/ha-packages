use std::{future::Future, pin::Pin};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{
    carrier::Carrier,
    models::{CarrierTracking, Status},
};

pub type ApiFuture<'a, T> =
    Pin<Box<dyn Future<Output = Result<T, crate::shipping_api::error::Error>> + Send + 'a>>;

/// Durable tracker ownership, separate from the package carrier. Add providers
/// only when an implementation and configuration are available.
#[derive(
    Clone, Copy, Debug, Default, Deserialize, Serialize, JsonSchema, PartialEq, Eq, sqlx::Type,
)]
#[serde(rename_all = "snake_case")]
#[sqlx(rename_all = "lowercase")]
pub enum ProviderId {
    #[default]
    #[serde(rename = "easypost")]
    EasyPost,
    #[serde(rename = "doordash")]
    DoorDash,
    #[serde(rename = "aftership")]
    AfterShip,
}

impl ProviderId {
    pub const fn for_carrier(carrier: Option<Carrier>) -> Self {
        match carrier {
            Some(Carrier::DoorDash) => Self::DoorDash,
            Some(Carrier::SpeedX | Carrier::Sway | Carrier::Veho) => Self::AfterShip,
            _ => Self::EasyPost,
        }
    }
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::EasyPost => "easypost",
            Self::DoorDash => "doordash",
            Self::AfterShip => "aftership",
        }
    }

    pub const fn name(self) -> &'static str {
        match self {
            Self::EasyPost => "EasyPost",
            Self::DoorDash => "DoorDash",
            Self::AfterShip => "AfterShip",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Registration {
    pub tracking_number: String,
    pub carrier: Option<Carrier>,
    pub recipient_url: Option<String>,
    /// Durable first attempt, preserved across uncertain results and restarts.
    pub started_at: i64,
}

/// Normalized observation. IDs are opaque to the worker; implementations must
/// validate identity and URLs before returning an observation.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Tracker {
    #[serde(default)]
    pub provider: ProviderId,
    pub id: String,
    pub tracking_code: String,
    pub carrier: Carrier,
    pub status: Status,
    pub public_url: Option<String>,
    pub carrier_tracking: CarrierTracking,
}
