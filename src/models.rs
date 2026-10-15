use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    #[default]
    Expected,
    Unknown,
    PreTransit,
    InTransit,
    OutForDelivery,
    AvailableForPickup,
    Delivered,
    ReturnToSender,
    Failure,
    Cancelled,
    Error,
}

impl Status {
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Delivered | Self::ReturnToSender | Self::Cancelled
        )
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Tracking {
    pub carrier: Option<String>,
    pub number: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Source {
    pub namespace: String,
    pub key: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CreateShipment {
    pub title: String,
    pub merchant: Option<String>,
    pub order_reference: Option<String>,
    pub tracking: Option<Tracking>,
    pub source: Option<Source>,
}

// Missing fields preserve their value; explicit null clears optional fields.
#[derive(Clone, Debug, JsonSchema)]
pub struct Patch<T> {
    pub value: Option<Option<T>>,
}

impl<T> Patch<T> {
    pub fn is_missing(&self) -> bool {
        self.value.is_none()
    }
}

impl<T: Serialize> Serialize for Patch<T> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match &self.value {
            Some(value) => value.serialize(serializer),
            None => serializer.serialize_none(),
        }
    }
}

impl<T> Default for Patch<T> {
    fn default() -> Self {
        Self { value: None }
    }
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for Patch<T> {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(Self {
            value: Some(Option::deserialize(deserializer)?),
        })
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct UpdateShipment {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Patch::is_missing")]
    #[schemars(with = "Option<String>")]
    pub merchant: Patch<String>,
    #[serde(default, skip_serializing_if = "Patch::is_missing")]
    #[schemars(with = "Option<String>")]
    pub order_reference: Patch<String>,
    #[serde(default, skip_serializing_if = "Patch::is_missing")]
    #[schemars(with = "Option<Tracking>")]
    pub tracking: Patch<Tracking>,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SetOverride {
    pub status: Status,
    pub reason: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
pub struct StatusOverride {
    pub status: Status,
    pub reason: String,
    pub actor_id: String,
    pub set_at: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
pub struct SyncState {
    pub tracker_id: Option<String>,
    pub last_success_at: Option<String>,
    pub last_error: Option<String>,
    pub next_attempt_at: Option<i64>,
    pub attempts: i64,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
pub struct Shipment {
    pub id: Uuid,
    pub revision: i64,
    pub title: String,
    pub merchant: Option<String>,
    pub order_reference: Option<String>,
    pub tracking: Option<Tracking>,
    pub status: Status,
    pub carrier_status: Option<Status>,
    pub status_override: Option<StatusOverride>,
    pub archived: bool,
    pub created_at: String,
    pub updated_at: String,
    pub sources: Vec<Source>,
    pub sync: SyncState,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    Created,
    Updated,
    SourceAdded,
    OverrideSet,
    OverrideCleared,
    Archived,
    Restored,
    RefreshRequested,
    CarrierUpdated,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
pub struct Event {
    pub id: i64,
    pub shipment_id: Uuid,
    pub kind: EventKind,
    pub actor_id: String,
    pub at: String,
    pub detail: String,
    pub snapshot: Option<Shipment>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema)]
pub struct Filters {
    pub tracking_number: Option<String>,
    pub carrier: Option<String>,
    pub order_reference: Option<String>,
    pub source_namespace: Option<String>,
    pub source_key: Option<String>,
    pub search: Option<String>,
    pub archived: Option<bool>,
    pub status: Option<Status>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema)]
pub struct Summary {
    pub active: i64,
    pub expected: i64,
    pub in_transit: i64,
    pub out_for_delivery: i64,
    pub delivered: i64,
    pub exceptions: i64,
    pub sync_errors: i64,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
pub struct System {
    pub version: String,
    pub provider_enabled: bool,
    pub pending_syncs: i64,
    pub sync_errors: i64,
    pub credential_role: String,
}
