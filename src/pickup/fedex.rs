//! FedEx Pickup Request REST v1. There is no documented pickup GET/list API.
//! A missing response is an unknown dispatch, never permission to book again.
//! https://developer.fedex.com/api/en-us/catalog/pickup/docs.html

use reqwest::Method;
use serde::{Deserialize, Serialize};

use crate::pickup::{
    AccountCredentials, Capabilities, Credentials, Error, ErrorCode, Lookup, Pickup, PickupFuture,
    Result,
    http::CarrierHttp,
    models::{Confirmation, Fee, Offer, PickupCarrier, PickupService, ResolvedPlan, validate_date},
};

#[derive(Clone)]
pub struct FedEx {
    http: CarrierHttp,
    account: String,
}
impl FedEx {
    #[cfg(test)]
    pub(crate) fn with_origin(mut self, origin: String) -> Self {
        self.http = self.http.with_origin(origin);
        self
    }
    pub fn new(config: AccountCredentials) -> Result<Self> {
        if config.account_number.len() != 9
            || !config.account_number.bytes().all(|c| c.is_ascii_digit())
        {
            return Err(Error::new(ErrorCode::InvalidInput));
        }
        Ok(Self {
            http: CarrierHttp::new(
                PickupCarrier::FedEx,
                Credentials {
                    client_id: config.client_id,
                    client_secret: config.client_secret,
                    environment: config.environment,
                },
                Some(&config.account_number),
            )?,
            account: config.account_number,
        })
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Address<'a> {
    street_lines: Vec<&'a str>,
    city: &'a str,
    state_or_province_code: &'a str,
    postal_code: &'a str,
    country_code: &'a str,
    residential: bool,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ShipmentAttributes {
    service_type: &'static str,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Availability<'a> {
    pickup_address: Address<'a>,
    dispatch_date: &'a str,
    package_ready_time: String,
    customer_close_time: String,
    pickup_type: &'static str,
    pickup_request_type: Vec<&'static str>,
    shipment_attributes: ShipmentAttributes,
    number_of_business_days: u8,
    associated_account_number: &'a str,
    carriers: [&'static str; 1],
    country_relationship: &'static str,
}
#[derive(Deserialize)]
struct Envelope<T> {
    output: T,
}
#[derive(Deserialize)]
struct AvailabilityOutput {
    options: Vec<ScheduleOption>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ScheduleOption {
    carrier: String,
    available: bool,
    pickup_date: String,
    cut_off_time: String,
    access_time: AccessTime,
    residential_available: Option<bool>,
}
#[derive(Deserialize)]
struct AccessTime {
    #[serde(default)]
    hours: u16,
    #[serde(default)]
    minutes: u16,
}

#[derive(Serialize)]
struct Account<'a> {
    value: &'a str,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Contact<'a> {
    person_name: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    company_name: Option<&'a str>,
    phone_number: String,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Party<'a> {
    contact: Contact<'a>,
    address: Address<'a>,
    delivery_instructions: String,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Origin<'a> {
    pickup_address_type: &'static str,
    pickup_location: Party<'a>,
    ready_date_timestamp: String,
    customer_close_time: String,
    package_location: &'static str,
}
#[derive(Serialize)]
struct Weight {
    units: &'static str,
    value: f64,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Responsible<'a> {
    account_number: Account<'a>,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Payor<'a> {
    responsible_party: Responsible<'a>,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Payment<'a> {
    payment_type: &'static str,
    payor: Payor<'a>,
    payor_type: &'static str,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Create<'a> {
    associated_account_number: Account<'a>,
    origin_detail: Origin<'a>,
    total_weight: Weight,
    package_count: u16,
    carrier_code: &'static str,
    pickup_type: &'static str,
    pickup_charges_payment: Payment<'a>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Created {
    pickup_confirmation_code: String,
    location: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Cancel<'a> {
    associated_account_number: Account<'a>,
    pickup_confirmation_code: &'a str,
    carrier_code: &'static str,
    scheduled_date: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    location: Option<&'a str>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Cancelled {
    pickup_confirmation_code: String,
    cancel_confirmation_message: String,
}

fn address(plan: &ResolvedPlan) -> Address<'_> {
    let a = &plan.site.details.address;
    let mut lines = vec![a.street.as_str()];
    if let Some(secondary) = &a.secondary {
        lines.push(secondary);
    }
    Address {
        street_lines: lines,
        city: &a.city,
        state_or_province_code: &a.state,
        postal_code: &a.postal_code,
        country_code: &a.country,
        residential: plan.site.details.residential,
    }
}
fn service(value: PickupService) -> Result<&'static str> {
    match value {
        PickupService::FedExGround => Ok("FEDEX_GROUND"),
        PickupService::FedExHomeDelivery => Ok("GROUND_HOME_DELIVERY"),
        PickupService::FedExStandardOvernight => Ok("STANDARD_OVERNIGHT"),
        PickupService::FedExPriorityOvernight => Ok("PRIORITY_OVERNIGHT"),
        PickupService::FedExFirstOvernight => Ok("FIRST_OVERNIGHT"),
        PickupService::FedExTwoDay => Ok("FEDEX_2_DAY"),
        PickupService::FedExExpressSaver => Ok("FEDEX_EXPRESS_SAVER"),
        PickupService::FedExInternationalPriority => Ok("INTERNATIONAL_PRIORITY"),
        PickupService::FedExInternationalEconomy => Ok("INTERNATIONAL_ECONOMY"),
        _ => Err(Error::new(ErrorCode::InvalidInput)),
    }
}
fn carrier(plan: &ResolvedPlan) -> Result<&'static str> {
    let group = plan
        .parcels
        .first()
        .ok_or_else(|| Error::new(ErrorCode::InvalidInput))?
        .service
        .group();
    if plan.carrier != PickupCarrier::FedEx
        || plan
            .parcels
            .iter()
            .any(|p| p.service.carrier() != PickupCarrier::FedEx || p.service.group() != group)
    {
        return Err(Error::new(ErrorCode::InvalidInput));
    }
    Ok(if group == "fedex_ground" {
        "FDXG"
    } else {
        "FDXE"
    })
}
fn minutes(value: &str) -> Option<u16> {
    let t = value.get(..5)?.parse::<jiff::civil::Time>().ok()?;
    Some(u16::try_from(t.hour()).ok()? * 60 + u16::try_from(t.minute()).ok()?)
}
fn safe_number(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 100
        && value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-')
}

impl Pickup for FedEx {
    fn carrier(&self) -> PickupCarrier {
        PickupCarrier::FedEx
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            lookup: Lookup::Unsupported,
            free_normal_route: false,
            custom_window: true,
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
            plan.site.details.validate()?;
            let operating = carrier(plan)?;
            let zone = jiff::tz::TimeZoneDatabase::bundled()
                .get(&plan.site.details.timezone)
                .map_err(|_| Error::new(ErrorCode::InvalidInput))?;
            let now = jiff::Timestamp::now().to_zoned(zone);
            let today = now.date().to_string();
            let date = plan.date.as_deref().unwrap_or(&today);
            validate_date(date)?;
            let mut offers: Option<Vec<Offer>> = None;
            for piece in &plan.parcels {
                let body = Availability {
                    pickup_address: address(plan),
                    dispatch_date: date,
                    package_ready_time: format!("{}:00", plan.ready_time),
                    customer_close_time: format!("{}:00", plan.close_time),
                    pickup_type: "ON_CALL",
                    pickup_request_type: if plan.date.is_some() {
                        vec![if date == today {
                            "SAME_DAY"
                        } else {
                            "FUTURE_DAY"
                        }]
                    } else {
                        vec!["SAME_DAY", "FUTURE_DAY"]
                    },
                    shipment_attributes: ShipmentAttributes {
                        service_type: service(piece.service)?,
                    },
                    number_of_business_days: if plan.date.is_some() { 1 } else { 14 },
                    associated_account_number: &self.account,
                    carriers: [operating],
                    country_relationship: if piece.destination_country == "US" {
                        "DOMESTIC"
                    } else {
                        "INTERNATIONAL"
                    },
                };
                let response = self
                    .http
                    .request(
                        Method::POST,
                        "/pickup/v1/pickups/availabilities",
                        &[],
                        Some(&body),
                        &[],
                        false,
                    )
                    .await?;
                let output: Envelope<AvailabilityOutput> = response.json(false)?;
                let mut current = Vec::new();
                for option in output.output.options {
                    if !option.available
                        || option.carrier != operating
                        || option.pickup_date < today
                        || plan.date.as_ref().is_some_and(|d| d != &option.pickup_date)
                        || (plan.site.details.residential
                            && option.residential_available == Some(false))
                        || validate_date(&option.pickup_date).is_err()
                    {
                        continue;
                    }
                    let mut ready = plan.ready_time.clone();
                    if option.pickup_date == today {
                        let local = format!("{:02}:{:02}", now.hour(), now.minute());
                        if ready < local {
                            ready = local;
                        }
                    }
                    let Some(ready_minutes) = minutes(&ready) else {
                        continue;
                    };
                    let Some(close_minutes) = minutes(&plan.close_time) else {
                        continue;
                    };
                    let Some(cutoff) = minutes(&option.cut_off_time) else {
                        continue;
                    };
                    let access = u32::from(option.access_time.hours) * 60
                        + u32::from(option.access_time.minutes);
                    if ready_minutes > cutoff
                        || close_minutes <= ready_minutes
                        || u32::from(close_minutes - ready_minutes) < access
                    {
                        continue;
                    }
                    current.push(Offer {
                        date: option.pickup_date,
                        ready_time: ready,
                        close_time: plan.close_time.clone(),
                        fee: Fee::Unquoted,
                    });
                }
                offers = Some(match offers {
                    None => current,
                    Some(previous) => previous
                        .into_iter()
                        .filter(|p| {
                            current.iter().any(|c| {
                                c.date == p.date
                                    && c.ready_time == p.ready_time
                                    && c.close_time == p.close_time
                            })
                        })
                        .collect(),
                });
            }
            let mut offers = offers.unwrap_or_default();
            offers.sort_by(|a, b| a.date.cmp(&b.date));
            offers
                .into_iter()
                .next()
                .ok_or_else(|| Error::new(ErrorCode::Ineligible))
        })
    }
    fn schedule<'a>(&'a self, plan: &'a ResolvedPlan) -> PickupFuture<'a, Confirmation> {
        Box::pin(async move {
            plan.site.details.validate()?;
            let operating = carrier(plan)?;
            if plan.parcels.is_empty() || plan.total_weight_hundredths == 0 {
                return Err(Error::new(ErrorCode::InvalidInput));
            }
            let date = plan
                .date
                .as_deref()
                .ok_or_else(|| Error::new(ErrorCode::InvalidInput))?;
            let site = &plan.site.details;
            let instructions = match &site.instructions {
                Some(notes) => format!("{}: {notes}", site.pickup_point),
                None => site.pickup_point.clone(),
            };
            // Delivery instructions have their own field; no truncation that could
            // silently remove access instructions or change the collection point.
            if instructions.len() > 255 {
                return Err(Error::new(ErrorCode::InvalidInput));
            }
            let body = Create {
                associated_account_number: Account {
                    value: &self.account,
                },
                origin_detail: Origin {
                    pickup_address_type: "OTHER",
                    pickup_location: Party {
                        contact: Contact {
                            person_name: &site.contact_name,
                            company_name: site.company.as_deref(),
                            phone_number: site.phone.chars().filter(char::is_ascii_digit).collect(),
                        },
                        address: address(plan),
                        delivery_instructions: instructions,
                    },
                    ready_date_timestamp: format!("{date}T{}:00", plan.ready_time),
                    customer_close_time: format!("{}:00", plan.close_time),
                    package_location: match site.pickup_point.as_str() {
                        "FRONT_DOOR" => "FRONT",
                        "BACK_DOOR" => "REAR",
                        "SIDE_DOOR" => "SIDE",
                        _ => "NONE",
                    },
                },
                total_weight: Weight {
                    units: "LB",
                    value: f64::from(plan.total_weight_hundredths) / 100.0,
                },
                package_count: plan.parcels.iter().try_fold(0_u16, |sum, p| {
                    sum.checked_add(p.count)
                        .ok_or_else(|| Error::new(ErrorCode::InvalidInput))
                })?,
                carrier_code: operating,
                pickup_type: "ON_CALL",
                pickup_charges_payment: Payment {
                    payment_type: "ACCOUNT",
                    payor: Payor {
                        responsible_party: Responsible {
                            account_number: Account {
                                value: &self.account,
                            },
                        },
                    },
                    payor_type: "SENDER",
                },
            };
            let response = self
                .http
                .request(
                    Method::POST,
                    "/pickup/v1/pickups",
                    &[],
                    Some(&body),
                    &[],
                    true,
                )
                .await?;
            let output: Envelope<Created> = response.json(true)?;
            let output = output.output;
            if !safe_number(&output.pickup_confirmation_code)
                || (operating == "FDXE" && output.location.as_ref().is_none_or(|s| !safe_number(s)))
            {
                return Err(Error::new(ErrorCode::UnknownOutcome));
            }
            Ok(Confirmation {
                number: output.pickup_confirmation_code,
                pickup_date: date.into(),
                location_code: output.location,
                etag: None,
            })
        })
    }
    fn lookup<'a>(
        &'a self,
        _plan: &'a ResolvedPlan,
        _saved: &'a Confirmation,
    ) -> PickupFuture<'a, Confirmation> {
        Box::pin(async { Err(Error::new(ErrorCode::Unsupported)) })
    }
    fn cancel<'a>(
        &'a self,
        plan: &'a ResolvedPlan,
        saved: &'a Confirmation,
    ) -> PickupFuture<'a, ()> {
        Box::pin(async move {
            let operating = carrier(plan)?;
            if !safe_number(&saved.number)
                || (operating == "FDXE"
                    && saved.location_code.as_ref().is_none_or(|s| !safe_number(s)))
            {
                return Err(Error::new(ErrorCode::InvalidInput));
            }
            let body = Cancel {
                associated_account_number: Account {
                    value: &self.account,
                },
                pickup_confirmation_code: &saved.number,
                carrier_code: operating,
                scheduled_date: &saved.pickup_date,
                location: saved.location_code.as_deref(),
            };
            let response = self
                .http
                .request(
                    Method::PUT,
                    "/pickup/v1/pickups/cancel",
                    &[],
                    Some(&body),
                    &[],
                    true,
                )
                .await?;
            let output: Envelope<Cancelled> = response.json(true)?;
            if output.output.pickup_confirmation_code != saved.number
                || output.output.cancel_confirmation_message.is_empty()
            {
                return Err(Error::new(ErrorCode::UnknownOutcome));
            }
            Ok(())
        })
    }
}
