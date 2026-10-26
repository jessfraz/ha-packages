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
    pub const TERMINAL: [Self; 3] = [Self::Delivered, Self::ReturnToSender, Self::Cancelled];

    pub fn is_terminal(self) -> bool {
        Self::TERMINAL.contains(&self)
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Tracking<C = crate::carrier::Carrier> {
    pub carrier: C,
    #[serde(default)]
    #[schemars(required)]
    pub number: String,
    /// Original recipient link. Required for DoorDash; omitted for other carriers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recipient_url: Option<String>,
}

/// Carrier detection is allowed at admission. Persisted shipments always use
/// the concrete carrier returned by the tracking provider.
#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct InputTracking {
    #[serde(
        default,
        deserialize_with = "input_carrier",
        serialize_with = "output_input_carrier"
    )]
    #[schemars(schema_with = "input_carrier_schema")]
    pub carrier: Option<crate::carrier::Carrier>,
    #[serde(default)]
    #[schemars(required)]
    pub number: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recipient_url: Option<String>,
}

// Resolve ordinary carrier names at admission, before deduplication and any
// provider operation. Persisted/provider Carrier values retain exact identity.
fn input_carrier<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<Option<crate::carrier::Carrier>, D::Error> {
    use crate::carrier::Carrier;
    Option::<String>::deserialize(d)?
        .map(|value| {
            if value.trim().eq_ignore_ascii_case("FedExAccount") {
                Ok(Carrier::FedEx)
            } else {
                value
                    .parse::<Carrier>()
                    .map(|carrier| match carrier {
                        Carrier::FedEx => Carrier::FedExDefault,
                        other => other,
                    })
                    .map_err(serde::de::Error::custom)
            }
        })
        .transpose()
}

fn output_input_carrier<S: serde::Serializer>(
    carrier: &Option<crate::carrier::Carrier>,
    s: S,
) -> Result<S::Ok, S::Error> {
    use crate::carrier::Carrier;
    carrier
        .map(|carrier| {
            if carrier == Carrier::FedEx {
                "FedExAccount"
            } else {
                carrier.as_str()
            }
        })
        .serialize(s)
}

fn input_carrier_schema(
    generator: &mut schemars::r#gen::SchemaGenerator,
) -> schemars::schema::Schema {
    use schemars::schema::{Schema, SchemaObject, SubschemaValidation};
    let exact = SchemaObject {
        instance_type: Some(schemars::schema::InstanceType::String.into()),
        enum_values: Some(vec![serde_json::json!("FedExAccount")]),
        ..Default::default()
    };
    SchemaObject {
        subschemas: Some(Box::new(SubschemaValidation {
            any_of: Some(vec![
                generator.subschema_for::<Option<crate::carrier::Carrier>>(),
                Schema::Object(exact),
            ]),
            ..Default::default()
        })),
        ..Default::default()
    }
    .into()
}

impl From<Tracking> for InputTracking {
    fn from(tracking: Tracking) -> Self {
        Self {
            carrier: Some(tracking.carrier),
            number: tracking.number,
            recipient_url: tracking.recipient_url,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Source {
    pub namespace: String,
    pub key: String,
}

#[derive(Clone, Copy, Debug, Default, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    #[default]
    Inbound,
    Outbound,
}

impl Direction {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Inbound => "inbound",
            Self::Outbound => "outbound",
        }
    }
}

impl<'de> Deserialize<'de> for Direction {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        match ProviderValue::<String>::deserialize(deserializer)? {
            ProviderValue::Value(value) if value == "inbound" => Ok(Self::Inbound),
            ProviderValue::Value(value) if value == "outbound" => Ok(Self::Outbound),
            _ => Err(serde::de::Error::custom(
                "direction must be inbound or outbound",
            )),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CreateShipment {
    pub title: String,
    pub direction: Direction,
    pub merchant: Option<String>,
    pub order_reference: Option<String>,
    /// Known from the order or carrier; missing means the requirement is unknown.
    pub signature_required: Option<bool>,
    #[serde(default, deserialize_with = "required_tracking")]
    #[schemars(required)]
    pub tracking: InputTracking,
    pub source: Option<Source>,
}

fn required_tracking<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<InputTracking, D::Error> {
    Ok(Option::<InputTracking>::deserialize(deserializer)?.unwrap_or_default())
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
    #[schemars(with = "Direction")]
    pub direction: Patch<Direction>,
    #[serde(default, skip_serializing_if = "Patch::is_missing")]
    #[schemars(with = "Option<String>")]
    pub merchant: Patch<String>,
    #[serde(default, skip_serializing_if = "Patch::is_missing")]
    #[schemars(with = "Option<String>")]
    pub order_reference: Patch<String>,
    #[serde(default, skip_serializing_if = "Patch::is_missing")]
    #[schemars(with = "Option<bool>")]
    pub signature_required: Patch<bool>,
    #[serde(default, skip_serializing_if = "Patch::is_missing")]
    #[schemars(with = "InputTracking")]
    pub tracking: Patch<InputTracking>,
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

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RegistrationState {
    #[default]
    Pending,
    Registered,
    Rejected,
    ConfigurationRequired,
}

#[derive(Clone, Debug, Serialize, JsonSchema)]
pub struct SyncState {
    pub provider: crate::shipping_api::types::ProviderId,
    pub registration_state: RegistrationState,
    pub registration_started_at: Option<i64>,
    pub error_code: Option<String>,
    pub provider_status: Option<u16>,
    pub provider_error_code: Option<String>,
    pub tracker_id: Option<String>,
    pub tracking_url: Option<String>,
    pub last_success_at: Option<String>,
    pub last_error: Option<String>,
    pub next_attempt_at: Option<i64>,
    pub attempts: i64,
}

impl<'de> Deserialize<'de> for SyncState {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct StoredSyncState {
            #[serde(default)]
            provider: crate::shipping_api::types::ProviderId,
            registration_state: Option<RegistrationState>,
            registration_started_at: Option<i64>,
            error_code: Option<String>,
            provider_status: Option<u16>,
            provider_error_code: Option<String>,
            tracker_id: Option<String>,
            tracking_url: Option<String>,
            last_success_at: Option<String>,
            last_error: Option<String>,
            next_attempt_at: Option<i64>,
            attempts: i64,
        }
        let value = StoredSyncState::deserialize(deserializer)?;
        let registration_state =
            value
                .registration_state
                .unwrap_or(if value.tracker_id.is_some() {
                    RegistrationState::Registered
                } else {
                    RegistrationState::Pending
                });
        Ok(Self {
            provider: value.provider,
            registration_state,
            registration_started_at: value.registration_started_at,
            error_code: value.error_code,
            provider_status: value.provider_status,
            provider_error_code: value.provider_error_code,
            tracker_id: value.tracker_id,
            tracking_url: value.tracking_url,
            last_success_at: value.last_success_at,
            last_error: value.last_error,
            next_attempt_at: value.next_attempt_at,
            attempts: value.attempts,
        })
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(default)]
pub struct CarrierTracking {
    pub events: Vec<CarrierEvent>,
    pub signed_by: Option<String>,
    pub estimated_delivery_at: Option<String>,
    /// Actual delivery scan time, independent of an estimated delivery date.
    pub delivered_at: Option<String>,
    pub events_truncated: bool,
}

impl CarrierTracking {
    pub(crate) const MAX_EVENTS: usize = 512;

    pub(crate) fn merge(&mut self, incoming: &Self) -> bool {
        let previous = self.clone();
        for event in &incoming.events {
            if !self.events.contains(event) {
                self.events.push(event.clone());
            }
        }
        self.events.sort_by_key(|event| {
            event.datetime.as_deref().and_then(|value| {
                time::OffsetDateTime::parse(value, &time::format_description::well_known::Rfc3339)
                    .ok()
                    .map(|at| at.unix_timestamp_nanos())
            })
        });
        if self.events.len() > Self::MAX_EVENTS {
            self.events.drain(..self.events.len() - Self::MAX_EVENTS);
            self.events_truncated = true;
        }
        self.events_truncated |= incoming.events_truncated;
        if let Some(delivered_at) = self.events.iter().rev().find_map(|event| {
            (event.status == Some(Status::Delivered))
                .then_some(event.datetime.as_ref())
                .flatten()
        }) {
            self.delivered_at = Some(delivered_at.clone());
        } else if let Some(delivered_at) = &incoming.delivered_at {
            self.delivered_at = Some(delivered_at.clone());
        }
        if let Some(signed_by) = &incoming.signed_by {
            self.signed_by = Some(signed_by.clone());
        }
        if let Some(estimated_delivery_at) = &incoming.estimated_delivery_at {
            self.estimated_delivery_at = Some(estimated_delivery_at.clone());
        }
        *self != previous
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(default)]
pub struct CarrierEvent {
    #[serde(deserialize_with = "provider_optional")]
    pub status: Option<Status>,
    #[serde(deserialize_with = "provider_optional")]
    pub status_detail: Option<String>,
    #[serde(deserialize_with = "provider_optional")]
    pub message: Option<String>,
    #[serde(deserialize_with = "provider_optional")]
    pub description: Option<String>,
    #[serde(deserialize_with = "provider_optional")]
    pub datetime: Option<String>,
    #[serde(deserialize_with = "provider_optional")]
    pub source: Option<String>,
    #[serde(deserialize_with = "provider_optional")]
    pub carrier_code: Option<String>,
    #[serde(deserialize_with = "provider_optional")]
    pub tracking_location: Option<TrackingLocation>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(default)]
pub struct TrackingLocation {
    #[serde(deserialize_with = "provider_optional")]
    pub city: Option<String>,
    #[serde(deserialize_with = "provider_optional")]
    pub state: Option<String>,
    #[serde(deserialize_with = "provider_optional")]
    pub country: Option<String>,
    #[serde(deserialize_with = "provider_optional")]
    pub zip: Option<String>,
}

// Carrier metadata is best effort. A malformed optional field must not reject
// an otherwise valid tracker or hide its other available scan details.
#[derive(Deserialize)]
#[serde(untagged)]
pub(crate) enum ProviderValue<T> {
    Value(T),
    Ignored(serde::de::IgnoredAny),
}

pub(crate) fn provider_optional<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Ok(match ProviderValue::<T>::deserialize(deserializer)? {
        ProviderValue::Value(value) => Some(value),
        ProviderValue::Ignored(_) => None,
    })
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
pub struct Shipment<T = Tracking> {
    pub id: Uuid,
    pub revision: i64,
    pub title: String,
    // Historical snapshots predate explicit direction. New creates require it.
    #[serde(default)]
    pub direction: Direction,
    pub merchant: Option<String>,
    pub order_reference: Option<String>,
    #[serde(default)]
    pub signature_required: Option<bool>,
    pub tracking: T,
    pub status: Status,
    pub carrier_status: Option<Status>,
    #[serde(default)]
    pub carrier_tracking: CarrierTracking,
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
    // Historical snapshots can predate required tracking numbers and carriers.
    pub snapshot: Option<Shipment<Option<Tracking<Option<crate::carrier::Carrier>>>>>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema)]
pub struct Filters {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub direction: Option<Direction>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tracking_number: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub carrier: Option<crate::carrier::Carrier>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub order_reference: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_namespace: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub search: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub archived: Option<bool>,
    /// Filter by whether the effective status is nonterminal.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<Status>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exceptions: Option<bool>,
    /// Filter by whether a shipment has a stored tracking failure, including retries.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tracking_issues: Option<bool>,
    /// Inclusive and exclusive UTC bounds over actual carrier delivery scans.
    /// Supply both bounds together.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delivered_after: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delivered_before: Option<i64>,
    /// Expected delivery first, or most recent actual delivery first. Unknown dates sort last.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sort: Option<ShipmentSort>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ShipmentSort {
    #[default]
    Id,
    Expected,
    Delivered,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
pub struct ShipmentPosition {
    pub id: Uuid,
    /// Captured from the selected row, so later edits cannot move the cursor boundary.
    pub sort_key: Option<i64>,
}

pub struct ShipmentListEntry {
    pub shipment: Shipment,
    pub position: ShipmentPosition,
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
    #[serde(default)]
    pub enabled_providers: Vec<crate::shipping_api::types::ProviderId>,
    pub pending_syncs: i64,
    pub sync_errors: i64,
    pub credential_role: String,
    pub delivery_day: Option<crate::delivery::DeliveryDay>,
}

#[derive(Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CreateAgent {
    pub name: String,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct AgentCredential {
    pub id: Uuid,
    pub name: String,
    pub created_at: String,
}

#[derive(Deserialize, Serialize, JsonSchema)]
pub struct AgentCredentials {
    pub items: Vec<AgentCredential>,
}

/// The plaintext credential is returned only by creation, never by listing.
#[derive(Deserialize, Serialize, JsonSchema)]
pub struct CreatedAgentCredential {
    pub credential: AgentCredential,
    pub token: String,
}

/// An effective state transition preserved for Home Assistant notifications.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StateEventType {
    OutForDelivery,
    Delivered,
    Exception,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
pub struct StateEvent {
    pub id: i64,
    pub shipment_id: Uuid,
    pub event_type: StateEventType,
    pub status: Status,
    pub title: String,
    pub carrier: Option<crate::carrier::Carrier>,
    pub direction: Direction,
    pub at: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
pub struct StateEventPage {
    pub items: Vec<StateEvent>,
    /// Persist only after delivering every item in this page. A crash can
    /// replay an event; its stable id lets consumers deduplicate it.
    pub next_after: i64,
    pub has_more: bool,
}

/// Small calendar projection; polling this never transfers tracking history.
#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
pub struct DeliveryEstimate {
    pub id: Uuid,
    pub title: String,
    pub carrier: Option<crate::carrier::Carrier>,
    pub direction: Direction,
    pub status: Status,
    pub estimated_delivery_at: String,
}
pub struct DeliveryEstimatePosition {
    pub id: Uuid,
    pub sort_key: i64,
}
pub struct DeliveryEstimateEntry {
    pub estimate: DeliveryEstimate,
    pub position: DeliveryEstimatePosition,
}
