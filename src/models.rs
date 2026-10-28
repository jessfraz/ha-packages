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
    /// Original provider ETA before civil-date normalization.
    pub estimated_delivery_raw: Option<String>,
    /// Explicit provider bounds; a lone ETA never implies a delivery window.
    #[serde(deserialize_with = "provider_optional")]
    pub estimated_delivery_window: Option<EstimatedDeliveryWindow>,
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
            self.estimated_delivery_raw = incoming.estimated_delivery_raw.clone();
        }
        // A successful provider snapshot is authoritative for its current
        // window, including withdrawal. Historical scans and overrides survive.
        self.estimated_delivery_window = incoming.estimated_delivery_window.clone();
        *self != previous
    }
}

/// An inclusive date range or an explicit aware-time interval, as supplied by
/// the carrier. Preserve offsets rather than inventing a timezone or endpoint.
#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(try_from = "DeliveryWindowBounds")]
pub struct EstimatedDeliveryWindow {
    pub start: String,
    pub end: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DeliveryWindowBounds {
    start: String,
    end: String,
}

impl TryFrom<DeliveryWindowBounds> for EstimatedDeliveryWindow {
    type Error = &'static str;

    fn try_from(value: DeliveryWindowBounds) -> Result<Self, Self::Error> {
        Self::new(value.start, value.end).ok_or("invalid estimated delivery window")
    }
}

impl EstimatedDeliveryWindow {
    pub(crate) fn new(start: String, end: String) -> Option<Self> {
        if start.len() > 64 || end.len() > 64 {
            return None;
        }
        let valid = if start.len() == 10 && end.len() == 10 {
            let format =
                time::format_description::parse_borrowed::<2>("[year]-[month]-[day]").ok()?;
            let start = time::Date::parse(&start, &format).ok()?;
            let end = time::Date::parse(&end, &format).ok()?;
            end >= start
        } else {
            let format = time::format_description::well_known::Rfc3339;
            let start = time::OffsetDateTime::parse(&start, &format).ok()?;
            let end = time::OffsetDateTime::parse(&end, &format).ok()?;
            end > start
        };
        valid.then_some(Self { start, end })
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
    pub estimated_delivery_at: Option<String>,
    #[serde(default)]
    pub estimated_delivery_window: Option<EstimatedDeliveryWindow>,
    /// Verified actual carrier delivery date/time, never an ETA or override time.
    pub delivered_at: Option<String>,
}
pub struct DeliveryEstimatePosition {
    pub id: Uuid,
    pub sort_key: i64,
}
pub struct DeliveryEstimateEntry {
    pub estimate: DeliveryEstimate,
    pub position: DeliveryEstimatePosition,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn estimated_delivery_windows_require_valid_explicit_paired_bounds() {
        for (start, end) in [
            ("2031-02-17", "2031-02-17"),
            ("2031-02-17", "2031-02-19"),
            ("2031-02-17T12:00:00-05:00", "2031-02-17T14:00:00-05:00"),
            ("2031-02-17T12:00:00-05:00", "2031-02-17T18:00:00Z"),
            ("2031-02-17T00:00:00-05:00", "2031-02-18T00:00:00-05:00"),
        ] {
            let raw = serde_json::json!({ "start": start, "end": end });
            let window: EstimatedDeliveryWindow = serde_json::from_value(raw.clone()).unwrap();
            assert_eq!(window.start, start);
            assert_eq!(window.end, end);
            assert_eq!(serde_json::to_value(window).unwrap(), raw);
        }
        for (start, end) in [
            ("2031-02-17", "2031-02-16"),
            ("2031-02-30", "2031-03-01"),
            ("2031-2-17", "2031-02-18"),
            ("2031-02-17", "2031-02-17T14:00:00Z"),
            ("2031-02-17T12:00:00", "2031-02-17T14:00:00"),
            ("2031-02-17T12:00:00Z", "2031-02-17T12:00:00Z"),
            ("2031-02-17T12:00:00-05:00", "2031-02-17T16:00:00Z"),
            ("2031-02-17T12:00:00Z", "malformed"),
            ("", "2031-02-17T12:00:00Z"),
        ] {
            assert!(
                serde_json::from_value::<EstimatedDeliveryWindow>(
                    serde_json::json!({ "start": start, "end": end })
                )
                .is_err(),
                "accepted invalid window {start} / {end}"
            );
        }
        for raw in [
            serde_json::json!({ "start": "2031-02-17" }),
            serde_json::json!({ "end": "2031-02-17" }),
            serde_json::json!({ "start": null, "end": "2031-02-17" }),
        ] {
            assert!(serde_json::from_value::<EstimatedDeliveryWindow>(raw.clone()).is_err());
            // Optional malformed carrier metadata must not hide other scans.
            let tracking: CarrierTracking = serde_json::from_value(serde_json::json!({
                "estimated_delivery_window": raw,
                "estimated_delivery_at": "2031-02-17T12:00:00Z"
            }))
            .unwrap();
            assert_eq!(tracking.estimated_delivery_window, None);
            assert!(tracking.estimated_delivery_at.is_some());
        }
    }

    #[test]
    fn successful_snapshots_replace_and_withdraw_windows_without_erasing_scan_history() {
        let event = CarrierEvent {
            status: Some(Status::Delivered),
            datetime: Some("2031-02-17T14:30:00-05:00".into()),
            ..Default::default()
        };
        let mut stored = CarrierTracking {
            events: vec![event.clone()],
            signed_by: Some("Recipient".into()),
            estimated_delivery_at: Some("2031-02-17T18:00:00Z".into()),
            estimated_delivery_window: EstimatedDeliveryWindow::new(
                "2031-02-17T12:00:00-05:00".into(),
                "2031-02-17T14:00:00-05:00".into(),
            ),
            ..Default::default()
        };
        let replacement = CarrierTracking {
            estimated_delivery_window: EstimatedDeliveryWindow::new(
                "2031-02-18".into(),
                "2031-02-19".into(),
            ),
            ..Default::default()
        };
        assert!(stored.merge(&replacement));
        assert_eq!(
            stored.estimated_delivery_window,
            replacement.estimated_delivery_window
        );
        let point_only = CarrierTracking {
            estimated_delivery_at: Some("2031-02-19T12:00:00Z".into()),
            ..Default::default()
        };
        assert!(stored.merge(&point_only));
        assert_eq!(stored.estimated_delivery_window, None);
        assert_eq!(
            stored.estimated_delivery_at,
            point_only.estimated_delivery_at
        );
        assert_eq!(stored.events, [event]);
        assert_eq!(stored.signed_by.as_deref(), Some("Recipient"));
        assert_eq!(
            stored.delivered_at.as_deref(),
            Some("2031-02-17T14:30:00-05:00")
        );
        assert!(!stored.merge(&point_only));
    }
}
