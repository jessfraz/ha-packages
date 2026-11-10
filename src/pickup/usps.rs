//! USPS Carrier Pickup 3.1.10, using the published Pickup 3.0 REST contract.
//! Normal-route pickup is free; this does not buy postage or arrange Pickup on
//! Demand. https://developers.usps.com/carrierpickupv3

use reqwest::Method;
use serde::{Deserialize, Serialize};

use crate::pickup::{
    Capabilities, Credentials, Error, ErrorCode, Lookup, Pickup, PickupFuture, Result,
    http::CarrierHttp,
    models::{Confirmation, Fee, Offer, PickupCarrier, PickupService, ResolvedPlan, validate_date},
};

#[derive(Clone)]
pub struct Usps {
    http: CarrierHttp,
}
impl Usps {
    #[cfg(test)]
    pub(crate) fn with_origin(mut self, origin: String) -> Self {
        self.http = self.http.with_origin(origin);
        self
    }
    pub fn new(credentials: Credentials) -> Result<Self> {
        Ok(Self {
            http: CarrierHttp::new(PickupCarrier::Usps, credentials, None)?,
        })
    }
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireAddress {
    street_address: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    secondary_address: Option<String>,
    city: String,
    state: String,
    #[serde(rename = "ZIPCode")]
    zip_code: String,
    #[serde(rename = "ZIPPlus4", skip_serializing_if = "Option::is_none")]
    zip_plus4: Option<String>,
}

#[derive(Serialize)]
struct Contact<'a> {
    email: &'a str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PickupAddress<'a> {
    first_name: &'a str,
    last_name: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    firm: Option<&'a str>,
    address: WireAddress,
    contact: Vec<Contact<'a>>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Package {
    package_type: &'static str,
    package_count: u16,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Location<'a> {
    package_location: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    special_instructions: Option<&'a str>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Create<'a> {
    pickup_date: &'a str,
    pickup_address: PickupAddress<'a>,
    packages: Vec<Package>,
    estimated_weight: f64,
    pickup_location: Location<'a>,
    next_available_pickup: bool,
}

#[derive(Deserialize)]
struct Eligibility {
    address: WireAddress,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireConfirmation {
    confirmation_number: String,
    pickup_date: String,
    carrier_pickup_request: Option<Echo>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Echo {
    pickup_address: Eligibility,
}

fn wire_address(plan: &ResolvedPlan) -> WireAddress {
    let address = &plan.site.details.address;
    let (zip, extension) = address
        .postal_code
        .split_once('-')
        .map_or((address.postal_code.as_str(), None), |(a, b)| {
            (a, Some(b.to_string()))
        });
    WireAddress {
        street_address: address.street.clone(),
        secondary_address: address.secondary.clone(),
        city: address.city.clone(),
        state: address.state.clone(),
        zip_code: zip.into(),
        zip_plus4: extension,
    }
}
fn normalized(value: &str) -> String {
    value
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '/')
        .flat_map(char::to_uppercase)
        .collect()
}
fn street_key(value: &str) -> String {
    value
        .split_whitespace()
        .map(|word| match normalized(word).as_str() {
            "STREET" => "ST".into(),
            "ROAD" => "RD".into(),
            "AVENUE" => "AVE".into(),
            "DRIVE" => "DR".into(),
            "COURT" => "CT".into(),
            "LANE" => "LN".into(),
            "BOULEVARD" => "BLVD".into(),
            "PLACE" => "PL".into(),
            "TERRACE" => "TER".into(),
            "CIRCLE" => "CIR".into(),
            "HIGHWAY" => "HWY".into(),
            "NORTH" => "N".into(),
            "SOUTH" => "S".into(),
            "EAST" => "E".into(),
            "WEST" => "W".into(),
            _ => normalized(word),
        })
        .collect::<Vec<String>>()
        .join(" ")
}
fn same_address(actual: &WireAddress, plan: &ResolvedPlan) -> bool {
    let expected = wire_address(plan);
    actual.zip_code == expected.zip_code
        && actual.state.eq_ignore_ascii_case(&expected.state)
        && street_key(&actual.street_address) == street_key(&expected.street_address)
        && normalized(actual.secondary_address.as_deref().unwrap_or(""))
            == normalized(expected.secondary_address.as_deref().unwrap_or(""))
}

fn check(plan: &ResolvedPlan) -> Result<()> {
    plan.site.details.validate()?;
    if plan.carrier != PickupCarrier::Usps
        || plan
            .parcels
            .iter()
            .any(|p| p.service.carrier() != PickupCarrier::Usps)
        || plan
            .site
            .details
            .email
            .as_ref()
            .is_none_or(|s| s.len() > 50 || !s.contains('@'))
        || plan.site.details.contact_name.split_whitespace().count() < 2
        || plan.site.details.address.street.len() > 50
        || plan.site.details.address.city.len() > 28
    {
        return Err(Error::new(ErrorCode::InvalidInput));
    }
    if ![
        "FRONT_DOOR",
        "BACK_DOOR",
        "SIDE_DOOR",
        "KNOCK_ON_DOOR",
        "MAIL_ROOM",
        "OFFICE",
        "PORCH",
        "RECEPTION",
        "MAILBOX",
        "OTHER",
    ]
    .contains(&plan.site.details.pickup_point.as_str())
    {
        return Err(Error::new(ErrorCode::InvalidInput));
    }
    if plan.site.details.pickup_point == "OTHER" && plan.site.details.instructions.is_none() {
        return Err(Error::new(ErrorCode::InvalidInput));
    }
    Ok(())
}

fn candidate_date(plan: &ResolvedPlan) -> Result<String> {
    if let Some(date) = &plan.date {
        validate_date(date)?;
        return Ok(date.clone());
    }
    let zone = jiff::tz::TimeZoneDatabase::bundled()
        .get("America/Chicago")
        .map_err(|_| Error::new(ErrorCode::Unavailable))?;
    let now = jiff::Timestamp::now().to_zoned(zone);
    let mut date = now.date();
    if now.hour() >= 2 {
        date = date
            .checked_add(jiff::Span::new().days(1))
            .map_err(|_| Error::new(ErrorCode::InvalidInput))?;
    }
    if date.weekday() == jiff::civil::Weekday::Sunday {
        date = date
            .checked_add(jiff::Span::new().days(1))
            .map_err(|_| Error::new(ErrorCode::InvalidInput))?;
    }
    Ok(date.to_string())
}

fn confirmation(
    wire: WireConfirmation,
    plan: &ResolvedPlan,
    etag: Option<String>,
    mutation: bool,
) -> Result<Confirmation> {
    let code = if mutation {
        ErrorCode::UnknownOutcome
    } else {
        ErrorCode::Unavailable
    };
    if wire.confirmation_number.is_empty()
        || wire.confirmation_number.len() > 200
        || !wire
            .confirmation_number
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-')
        || validate_date(&wire.pickup_date).is_err()
        || wire
            .carrier_pickup_request
            .as_ref()
            .is_some_and(|echo| !same_address(&echo.pickup_address.address, plan))
    {
        return Err(Error::new(code));
    }
    Ok(Confirmation {
        number: wire.confirmation_number,
        pickup_date: wire.pickup_date,
        location_code: None,
        etag,
    })
}

impl Pickup for Usps {
    fn carrier(&self) -> PickupCarrier {
        PickupCarrier::Usps
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            lookup: Lookup::Confirmation,
            free_normal_route: true,
            custom_window: false,
            services: PickupService::ALL
                .into_iter()
                .filter(|s| s.carrier() == self.carrier())
                .collect(),
        }
    }
    fn fingerprint(&self) -> [u8; 32] {
        self.http.fingerprint()
    }
    fn preview<'a>(&'a self, plan: &'a ResolvedPlan) -> PickupFuture<'a, Offer> {
        Box::pin(async move {
            check(plan)?;
            let address = wire_address(plan);
            let mut query = vec![
                ("streetAddress".into(), address.street_address),
                ("city".into(), address.city),
                ("state".into(), address.state),
                ("ZIPCode".into(), address.zip_code),
            ];
            if let Some(secondary) = address.secondary_address {
                query.push(("secondaryAddress".into(), secondary));
            }
            if let Some(extension) = address.zip_plus4 {
                query.push(("ZIPPlus4".into(), extension));
            }
            let response = self
                .http
                .request::<()>(
                    Method::GET,
                    "/pickup/v3/carrier-pickup/eligibility",
                    &query,
                    None,
                    &[],
                    false,
                )
                .await?;
            let eligibility: Eligibility = response.json(false)?;
            if !same_address(&eligibility.address, plan) {
                return Err(Error::new(ErrorCode::Conflict));
            }
            Ok(Offer {
                date: candidate_date(plan)?,
                ready_time: plan.ready_time.clone(),
                close_time: plan.close_time.clone(),
                fee: Fee::Free,
            })
        })
    }
    fn schedule<'a>(&'a self, plan: &'a ResolvedPlan) -> PickupFuture<'a, Confirmation> {
        Box::pin(async move {
            check(plan)?;
            if plan.parcels.is_empty() || plan.total_weight_hundredths == 0 {
                return Err(Error::new(ErrorCode::InvalidInput));
            }
            let date = plan
                .date
                .as_deref()
                .ok_or_else(|| Error::new(ErrorCode::InvalidInput))?;
            let name = plan.site.details.contact_name.trim();
            let (first, last) = name
                .split_once(char::is_whitespace)
                .ok_or_else(|| Error::new(ErrorCode::InvalidInput))?;
            let email = plan
                .site
                .details
                .email
                .as_deref()
                .ok_or_else(|| Error::new(ErrorCode::InvalidInput))?;
            let packages = plan
                .parcels
                .iter()
                .map(|piece| Package {
                    package_type: match piece.service {
                        PickupService::UspsPriorityMail => "PRIORITY_MAIL",
                        PickupService::UspsPriorityMailExpress => "PRIORITY_MAIL_EXPRESS",
                        PickupService::UspsGroundAdvantage => "USPS_GROUND_ADVANTAGE",
                        PickupService::UspsReturns => "RETURNS",
                        _ => "INTERNATIONAL",
                    },
                    package_count: piece.count,
                })
                .collect();
            let body = Create {
                pickup_date: date,
                pickup_address: PickupAddress {
                    first_name: first,
                    last_name: last.trim(),
                    firm: plan.site.details.company.as_deref(),
                    address: wire_address(plan),
                    contact: vec![Contact { email }],
                },
                packages,
                estimated_weight: f64::from(plan.total_weight_hundredths) / 100.0,
                pickup_location: Location {
                    package_location: &plan.site.details.pickup_point,
                    special_instructions: plan.site.details.instructions.as_deref(),
                },
                next_available_pickup: plan.next_available,
            };
            let response = self
                .http
                .request(
                    Method::POST,
                    "/pickup/v3/carrier-pickup",
                    &[],
                    Some(&body),
                    &[],
                    true,
                )
                .await?;
            let etag = response
                .headers
                .get("etag")
                .and_then(|h| h.to_str().ok())
                .map(str::to_owned);
            confirmation(response.json(true)?, plan, etag, true)
        })
    }
    fn lookup<'a>(
        &'a self,
        plan: &'a ResolvedPlan,
        saved: &'a Confirmation,
    ) -> PickupFuture<'a, Confirmation> {
        Box::pin(async move {
            if saved.number.is_empty()
                || !saved
                    .number
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || c == b'-')
            {
                return Err(Error::new(ErrorCode::InvalidInput));
            }
            let response = self
                .http
                .request::<()>(
                    Method::GET,
                    &format!("/pickup/v3/carrier-pickup/{}", saved.number),
                    &[],
                    None,
                    &[],
                    false,
                )
                .await?;
            let mut rows: Vec<WireConfirmation> = response.json(false)?;
            if rows.len() != 1 {
                return Err(Error::new(ErrorCode::Conflict));
            }
            let row = rows.pop().ok_or_else(|| Error::new(ErrorCode::NotFound))?;
            if row.confirmation_number != saved.number || row.pickup_date != saved.pickup_date {
                return Err(Error::new(ErrorCode::Conflict));
            }
            let etag = response
                .headers
                .get("etag")
                .and_then(|h| h.to_str().ok())
                .map(str::to_owned);
            confirmation(row, plan, etag, false)
        })
    }
    fn cancel<'a>(
        &'a self,
        plan: &'a ResolvedPlan,
        saved: &'a Confirmation,
    ) -> PickupFuture<'a, ()> {
        Box::pin(async move {
            let current = self.lookup(plan, saved).await?;
            let etag = current
                .etag
                .as_deref()
                .ok_or_else(|| Error::new(ErrorCode::Unavailable))?;
            self.http
                .request::<()>(
                    Method::DELETE,
                    &format!("/pickup/v3/carrier-pickup/{}", current.number),
                    &[],
                    None,
                    &[("If-Match", etag)],
                    true,
                )
                .await?;
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use crate::pickup::usps::street_key;
    #[test]
    fn postal_suffixes_can_standardize_without_erasing_house_number_boundaries() {
        assert_eq!(
            street_key("123 Example Street"),
            street_key("123 EXAMPLE ST.")
        );
        assert_ne!(
            street_key("123-4 Example Street"),
            street_key("1234 Example Street")
        );
        assert_ne!(
            street_key("123 1/2 Example Street"),
            street_key("123 12 Example Street")
        );
    }
}
