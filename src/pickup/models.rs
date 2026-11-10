use std::str::FromStr;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::pickup::{Error, ErrorCode, Result};

#[derive(
    Clone,
    Copy,
    Debug,
    Deserialize,
    Serialize,
    JsonSchema,
    PartialEq,
    Eq,
    Hash,
    clap::ValueEnum,
    sqlx::Type,
)]
#[serde(rename_all = "snake_case")]
#[sqlx(rename_all = "snake_case")]
pub enum PickupCarrier {
    Usps,
    Ups,
    #[serde(rename = "fedex")]
    #[value(name = "fedex")]
    #[sqlx(rename = "fedex")]
    FedEx,
}

impl PickupCarrier {
    pub const ALL: [Self; 3] = [Self::Usps, Self::Ups, Self::FedEx];
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Usps => "usps",
            Self::Ups => "ups",
            Self::FedEx => "fedex",
        }
    }
    pub fn from_tracking(value: crate::carrier::Carrier) -> Option<Self> {
        use crate::carrier::Carrier;
        match value {
            Carrier::Usps | Carrier::UspsShip => Some(Self::Usps),
            Carrier::Ups | Carrier::UpsDap => Some(Self::Ups),
            Carrier::FedEx | Carrier::FedExDefault | Carrier::FedexSmartPost => Some(Self::FedEx),
            _ => None,
        }
    }
}

impl std::fmt::Display for PickupCarrier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for PickupCarrier {
    type Err = Error;
    fn from_str(value: &str) -> Result<Self> {
        match value {
            "usps" => Ok(Self::Usps),
            "ups" => Ok(Self::Ups),
            "fedex" => Ok(Self::FedEx),
            _ => Err(invalid()),
        }
    }
}

#[derive(
    Clone, Copy, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq, Hash, clap::ValueEnum,
)]
#[serde(rename_all = "snake_case")]
pub enum PickupService {
    UspsPriorityMail,
    UspsPriorityMailExpress,
    UspsGroundAdvantage,
    UspsPriorityMailInternational,
    UspsPriorityMailExpressInternational,
    UspsFirstClassPackageInternational,
    UspsReturns,
    UpsGround,
    UpsNextDayAir,
    UpsSecondDayAir,
    UpsThreeDaySelect,
    UpsWorldwideExpress,
    UpsWorldwideExpedited,
    #[serde(rename = "fedex_ground")]
    #[value(name = "fedex-ground")]
    FedExGround,
    #[serde(rename = "fedex_home_delivery")]
    #[value(name = "fedex-home-delivery")]
    FedExHomeDelivery,
    #[serde(rename = "fedex_standard_overnight")]
    #[value(name = "fedex-standard-overnight")]
    FedExStandardOvernight,
    #[serde(rename = "fedex_priority_overnight")]
    #[value(name = "fedex-priority-overnight")]
    FedExPriorityOvernight,
    #[serde(rename = "fedex_first_overnight")]
    #[value(name = "fedex-first-overnight")]
    FedExFirstOvernight,
    #[serde(rename = "fedex_two_day")]
    #[value(name = "fedex-two-day")]
    FedExTwoDay,
    #[serde(rename = "fedex_express_saver")]
    #[value(name = "fedex-express-saver")]
    FedExExpressSaver,
    #[serde(rename = "fedex_international_priority")]
    #[value(name = "fedex-international-priority")]
    FedExInternationalPriority,
    #[serde(rename = "fedex_international_economy")]
    #[value(name = "fedex-international-economy")]
    FedExInternationalEconomy,
}

impl PickupService {
    pub const ALL: [Self; 22] = [
        Self::UspsPriorityMail,
        Self::UspsPriorityMailExpress,
        Self::UspsGroundAdvantage,
        Self::UspsPriorityMailInternational,
        Self::UspsPriorityMailExpressInternational,
        Self::UspsFirstClassPackageInternational,
        Self::UspsReturns,
        Self::UpsGround,
        Self::UpsNextDayAir,
        Self::UpsSecondDayAir,
        Self::UpsThreeDaySelect,
        Self::UpsWorldwideExpress,
        Self::UpsWorldwideExpedited,
        Self::FedExGround,
        Self::FedExHomeDelivery,
        Self::FedExStandardOvernight,
        Self::FedExPriorityOvernight,
        Self::FedExFirstOvernight,
        Self::FedExTwoDay,
        Self::FedExExpressSaver,
        Self::FedExInternationalPriority,
        Self::FedExInternationalEconomy,
    ];
    pub const fn carrier(self) -> PickupCarrier {
        match self {
            Self::UspsPriorityMail
            | Self::UspsPriorityMailExpress
            | Self::UspsGroundAdvantage
            | Self::UspsPriorityMailInternational
            | Self::UspsPriorityMailExpressInternational
            | Self::UspsFirstClassPackageInternational
            | Self::UspsReturns => PickupCarrier::Usps,
            Self::UpsGround
            | Self::UpsNextDayAir
            | Self::UpsSecondDayAir
            | Self::UpsThreeDaySelect
            | Self::UpsWorldwideExpress
            | Self::UpsWorldwideExpedited => PickupCarrier::Ups,
            Self::FedExGround
            | Self::FedExHomeDelivery
            | Self::FedExStandardOvernight
            | Self::FedExPriorityOvernight
            | Self::FedExFirstOvernight
            | Self::FedExTwoDay
            | Self::FedExExpressSaver
            | Self::FedExInternationalPriority
            | Self::FedExInternationalEconomy => PickupCarrier::FedEx,
        }
    }
    pub const fn group(self) -> &'static str {
        match self {
            Self::FedExGround | Self::FedExHomeDelivery => "fedex_ground",
            _ if matches!(self.carrier(), PickupCarrier::FedEx) => "fedex_express",
            _ => self.carrier().as_str(),
        }
    }
}

#[derive(Clone, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Address {
    pub street: String,
    pub secondary: Option<String>,
    pub city: String,
    pub state: String,
    pub postal_code: String,
    pub country: String,
}

#[derive(Clone, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SiteInput {
    pub name: String,
    pub contact_name: String,
    pub company: Option<String>,
    pub phone: String,
    pub email: Option<String>,
    pub address: Address,
    pub timezone: String,
    pub ready_time: String,
    pub close_time: String,
    pub pickup_point: String,
    pub instructions: Option<String>,
    #[serde(default = "default_true")]
    pub residential: bool,
}

#[derive(Clone, Deserialize, Serialize, JsonSchema)]
pub struct Site {
    pub id: Uuid,
    pub revision: i64,
    pub is_default: bool,
    pub details: SiteInput,
}

#[derive(Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SaveSite {
    pub location: SiteInput,
    #[serde(default = "default_true")]
    pub make_default: bool,
}

fn valid_zip(value: &str) -> bool {
    value.len() == 5 && value.bytes().all(|c| c.is_ascii_digit())
        || value.len() == 10
            && value.as_bytes()[5] == b'-'
            && value
                .bytes()
                .enumerate()
                .all(|(i, c)| i == 5 || c.is_ascii_digit())
}

fn default_true() -> bool {
    true
}

impl SiteInput {
    pub fn validate(&self) -> Result<()> {
        for (value, max) in [
            (&self.name, 80),
            (&self.contact_name, 70),
            (&self.phone, 30),
            (&self.address.street, 70),
            (&self.address.city, 50),
            (&self.pickup_point, 40),
        ] {
            text(value, max)?;
        }
        for (value, max) in [
            (&self.company, 70),
            (&self.email, 200),
            (&self.address.secondary, 50),
            (&self.instructions, 255),
        ] {
            if let Some(value) = value {
                text(value, max)?;
            }
        }
        if self.address.country != "US"
            || self.address.state.len() != 2
            || !self.address.state.bytes().all(|c| c.is_ascii_uppercase())
            || !valid_zip(&self.address.postal_code)
        {
            return Err(invalid());
        }
        if self.phone.bytes().filter(u8::is_ascii_digit).count() < 10 {
            return Err(invalid());
        }
        if jiff::tz::TimeZoneDatabase::bundled()
            .get(&self.timezone)
            .is_err()
        {
            return Err(invalid());
        }
        validate_window(&self.ready_time, &self.close_time)
    }
}

#[derive(Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ParcelInput {
    pub carrier: PickupCarrier,
    pub service: PickupService,
    pub weight_lbs: String,
    pub destination_country: String,
}

impl ParcelInput {
    pub fn validate(&self) -> Result<u32> {
        if self.service.carrier() != self.carrier
            || self.destination_country.len() != 2
            || !self
                .destination_country
                .bytes()
                .all(|c| c.is_ascii_uppercase())
        {
            return Err(invalid());
        }
        parse_weight(&self.weight_lbs)
    }
}

#[derive(Clone, Deserialize, Serialize, JsonSchema)]
pub struct Parcel {
    pub tracking: String,
    pub details: ParcelInput,
}

/// Optional UI precondition. Normal CLI scheduling does not require a preview.
#[derive(Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReviewedSite {
    pub id: Uuid,
    pub revision: i64,
}

#[derive(Clone, Default, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ScheduleRequest {
    #[serde(default)]
    pub tracking: Vec<String>,
    pub carrier: Option<PickupCarrier>,
    pub site_id: Option<Uuid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reviewed_site: Option<ReviewedSite>,
    /// Optional preview precondition; one-command CLI scheduling omits it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reviewed_configuration: Option<Uuid>,
    pub date: Option<String>,
    pub ready_time: Option<String>,
    pub close_time: Option<String>,
    pub service: Option<PickupService>,
    pub weight_lbs: Option<String>,
    pub pieces: Option<u16>,
    pub destination_country: Option<String>,
    #[serde(default)]
    pub accept_fee: bool,
}

#[derive(Clone, Deserialize, Serialize, JsonSchema)]
pub struct ParcelGroup {
    pub service: PickupService,
    pub count: u16,
    pub destination_country: String,
}

/// An immutable, reviewed dispatch snapshot, including the saved location
/// revision. Billing identity remains in the carrier client, not user input.
#[derive(Clone, Deserialize, Serialize, JsonSchema)]
pub struct ResolvedPlan {
    pub reference: Uuid,
    pub carrier: PickupCarrier,
    pub site: Site,
    pub date: Option<String>,
    pub next_available: bool,
    pub ready_time: String,
    pub close_time: String,
    pub parcels: Vec<ParcelGroup>,
    pub total_weight_hundredths: u32,
    pub overweight: bool,
    pub tracking: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Fee {
    Free,
    Estimate { currency: String, amount: String },
    Unquoted,
}

#[derive(Clone, Deserialize, Serialize, JsonSchema)]
pub struct Offer {
    pub date: String,
    pub ready_time: String,
    pub close_time: String,
    pub fee: Fee,
}

#[derive(Clone, Deserialize, Serialize, JsonSchema)]
pub struct Preview {
    pub plan: ResolvedPlan,
    pub offer: Offer,
    pub configuration_id: Uuid,
}

#[derive(Clone, Deserialize, Serialize, JsonSchema)]
pub struct Confirmation {
    pub number: String,
    pub pickup_date: String,
    pub location_code: Option<String>,
    pub etag: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq, sqlx::Type)]
#[serde(rename_all = "snake_case")]
#[sqlx(rename_all = "snake_case")]
pub enum State {
    Pending,
    Dispatching,
    Scheduled,
    CancelPending,
    Cancelling,
    Cancelled,
    Rejected,
    Unknown,
}

#[derive(Clone, Deserialize, Serialize, JsonSchema)]
pub struct Record {
    pub id: Uuid,
    pub batch_id: Uuid,
    pub revision: i64,
    pub state: State,
    pub accept_fee: bool,
    pub plan: ResolvedPlan,
    pub offer: Option<Offer>,
    pub confirmation: Option<Confirmation>,
    pub error: Option<Error>,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Clone, Deserialize, Serialize, JsonSchema)]
pub struct Batch {
    pub id: Uuid,
    pub items: Vec<Record>,
}

pub(crate) fn invalid() -> Error {
    Error::new(ErrorCode::InvalidInput)
}
pub(crate) fn text(value: &str, max: usize) -> Result<()> {
    if value.trim().is_empty() || value.len() > max || value.chars().any(char::is_control) {
        return Err(invalid());
    }
    Ok(())
}

pub fn normalize_tracking(value: &str) -> Result<String> {
    text(value, 100)?;
    let value = value
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect::<String>()
        .to_ascii_uppercase();
    if !(6..=100).contains(&value.len())
        || !value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-')
        || !value.bytes().any(|c| c.is_ascii_digit())
    {
        return Err(invalid());
    }
    Ok(value)
}

pub fn parse_weight(value: &str) -> Result<u32> {
    let (whole, fraction) = value.split_once('.').unwrap_or((value, ""));
    if whole.is_empty()
        || whole.len() > 6
        || fraction.len() > 2
        || !whole
            .bytes()
            .chain(fraction.bytes())
            .all(|c| c.is_ascii_digit())
    {
        return Err(invalid());
    }
    let whole = whole.parse::<u32>().map_err(|_| invalid())?;
    let fraction = match fraction.len() {
        0 => 0,
        1 => fraction.parse::<u32>().map_err(|_| invalid())? * 10,
        _ => fraction.parse::<u32>().map_err(|_| invalid())?,
    };
    let hundredths = whole
        .checked_mul(100)
        .and_then(|n| n.checked_add(fraction))
        .filter(|n| *n > 0 && *n <= 7_000_000)
        .ok_or_else(invalid)?;
    Ok(hundredths)
}

pub fn weight_string(hundredths: u32) -> String {
    format!("{}.{:02}", hundredths / 100, hundredths % 100)
}

pub fn validate_date(value: &str) -> Result<()> {
    if value.len() != 10 || value.parse::<jiff::civil::Date>().is_err() {
        return Err(invalid());
    }
    Ok(())
}

pub fn validate_window(ready: &str, close: &str) -> Result<()> {
    if ready.len() != 5
        || close.len() != 5
        || ready.parse::<jiff::civil::Time>().is_err()
        || close.parse::<jiff::civil::Time>().is_err()
        || ready >= close
    {
        return Err(invalid());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::pickup::models::{PickupCarrier, normalize_tracking, parse_weight, validate_window};

    #[test]
    fn pickup_carriers_are_only_direct_implementations_and_have_canonical_names() {
        for (carrier, name) in [
            (PickupCarrier::Usps, "usps"),
            (PickupCarrier::Ups, "ups"),
            (PickupCarrier::FedEx, "fedex"),
        ] {
            assert_eq!(
                serde_json::to_string(&carrier).unwrap(),
                format!("\"{name}\"")
            );
            assert_eq!(name.parse::<PickupCarrier>().unwrap(), carrier);
        }
        for value in ["FedExDefault", "UPSDAP", "usps_ship", "amazon_account"] {
            assert!(value.parse::<PickupCarrier>().is_err());
        }
    }

    #[test]
    fn weight_and_tracking_normalization_are_deterministic_and_reject_invalid_input() {
        assert_eq!(parse_weight("3.4").unwrap(), 340);
        assert_eq!(parse_weight("0.01").unwrap(), 1);
        for value in ["0", "-1", "NaN", "2.345", "1e6", ".5", "1.2.3"] {
            assert!(parse_weight(value).is_err());
        }
        assert_eq!(normalize_tracking("1z 0000-0000").unwrap(), "1Z0000-0000");
        assert!(normalize_tracking("../../secret").is_err());
        assert!(validate_window("17:00", "09:00").is_err());
        assert!(validate_window("09:00", "17:00").is_ok());
    }
}

/// Recorder-safe projection: no addresses, contacts, barcodes or confirmations.
#[derive(Clone, Deserialize, Serialize, JsonSchema)]
pub struct PickupWindow {
    pub id: Uuid,
    pub carrier: PickupCarrier,
    pub date: String,
    pub ready_time: String,
    pub close_time: String,
    pub timezone: String,
}
#[derive(Clone, Deserialize, Serialize, JsonSchema)]
pub struct PickupStatus {
    pub scheduled: u32,
    pub uncertain: u32,
    pub windows: Vec<PickupWindow>,
    pub windows_truncated: bool,
}

/// An explicit operator assertion after checking the carrier portal/support.
/// This is not an automatic inference from an empty or unavailable API result.
#[derive(Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReconcileRequest {
    pub carrier_portal_checked: bool,
    pub outcome: ReconcileOutcome,
}
#[derive(Clone, Deserialize, Serialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ReconcileOutcome {
    Scheduled { confirmation: Confirmation },
    NotScheduled,
    Cancelled,
}
