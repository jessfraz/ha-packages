//! UPS Pickup REST v2409. Account billing only; no card collection or alternate
//! payment fallback. transId is correlation, not provider idempotency.
//! REST uses the spec's /api server base; OAuth stays at /security on the host.
//! https://github.com/UPS-API/api-documentation/blob/main/Pickup.yaml

use reqwest::Method;
use serde::{Deserialize, Serialize};

use crate::pickup::{
    AccountCredentials, Capabilities, Credentials, Error, ErrorCode, Lookup, Pickup, PickupFuture,
    Result,
    http::CarrierHttp,
    models::{
        Confirmation, Fee, Offer, PickupCarrier, PickupService, ResolvedPlan, validate_date,
        weight_string,
    },
};

#[derive(Clone)]
pub struct Ups {
    http: CarrierHttp,
    account: String,
}
impl Ups {
    #[cfg(test)]
    pub(crate) fn with_origin(mut self, origin: String) -> Self {
        self.http = self.http.with_origin(origin);
        self
    }
    pub fn new(config: AccountCredentials) -> Result<Self> {
        if ![6, 10].contains(&config.account_number.len()) {
            return Err(Error::new(ErrorCode::InvalidInput));
        }
        Ok(Self {
            http: CarrierHttp::new(
                PickupCarrier::Ups,
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
struct Request {}
#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct Account<'a> {
    account_number: &'a str,
    account_country_code: &'static str,
}
#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct Shipper<'a> {
    account: Account<'a>,
}
#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct DateInfo {
    pickup_date: String,
    ready_time: String,
    close_time: String,
}
#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct RateAddress<'a> {
    city: &'a str,
    state_province: &'a str,
    postal_code: &'a str,
    country_code: &'a str,
    residential_indicator: &'static str,
}
#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct RateRequest<'a> {
    request: Request,
    shipper_account: Account<'a>,
    pickup_address: RateAddress<'a>,
    alternate_address_indicator: &'static str,
    service_date_option: &'static str,
    pickup_date_info: DateInfo,
    tax_information_indicator: &'static str,
}
#[derive(Serialize)]
struct RateEnvelope<'a> {
    #[serde(rename = "PickupRateRequest")]
    request: RateRequest<'a>,
}

#[derive(Deserialize)]
struct Status {
    #[serde(rename = "Code")]
    code: String,
}
#[derive(Deserialize)]
struct ResponseStatus {
    #[serde(rename = "ResponseStatus")]
    status: Status,
}
#[derive(Deserialize)]
struct RateResult {
    #[serde(rename = "CurrencyCode")]
    currency: String,
    #[serde(rename = "GrandTotalOfAllCharge")]
    total: Option<String>,
    #[serde(rename = "GrandTotalOfAllIncentedCharge")]
    discounted_total: Option<String>,
}
#[derive(Deserialize)]
struct RateResponse {
    #[serde(rename = "Response")]
    response: ResponseStatus,
    #[serde(rename = "RateResult")]
    rate: RateResult,
}
#[derive(Deserialize)]
struct RateOutput {
    #[serde(rename = "PickupRateResponse")]
    response: RateResponse,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct Phone {
    number: String,
}
#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct PickupAddress<'a> {
    company_name: &'a str,
    contact_name: &'a str,
    address_line: Vec<&'a str>,
    city: &'a str,
    state_province: &'a str,
    postal_code: &'a str,
    country_code: &'a str,
    residential_indicator: &'static str,
    pickup_point: &'a str,
    phone: Phone,
}
#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct Piece<'a> {
    service_code: &'static str,
    quantity: String,
    destination_country_code: &'a str,
    container_code: &'static str,
}
#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct Weight {
    weight: String,
    unit_of_measurement: &'static str,
}
#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct Create<'a> {
    request: Request,
    rate_pickup_indicator: &'static str,
    shipper: Shipper<'a>,
    pickup_date_info: DateInfo,
    pickup_address: PickupAddress<'a>,
    alternate_address_indicator: &'static str,
    pickup_piece: Vec<Piece<'a>>,
    total_weight: Weight,
    payment_method: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    overweight_indicator: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    special_instruction: Option<&'a str>,
    reference_number: String,
    shipping_labels_available: &'static str,
}
#[derive(Serialize)]
struct CreateEnvelope<'a> {
    #[serde(rename = "PickupCreationRequest")]
    request: Create<'a>,
}
#[derive(Deserialize)]
struct CreateResponse {
    #[serde(rename = "Response")]
    response: ResponseStatus,
    #[serde(rename = "PRN")]
    prn: Option<String>,
}
#[derive(Deserialize)]
struct CreateOutput {
    #[serde(rename = "PickupCreationResponse")]
    response: CreateResponse,
}

#[derive(Deserialize)]
struct Pending {
    #[serde(rename = "PRN")]
    prn: String,
    #[serde(rename = "ServiceDate")]
    date: String,
    #[serde(rename = "ReferenceNumber")]
    reference: Option<String>,
}
#[derive(Deserialize)]
struct PendingResponse {
    #[serde(rename = "Response")]
    response: ResponseStatus,
    #[serde(rename = "PendingStatus")]
    pending: Vec<Pending>,
}
#[derive(Deserialize)]
struct PendingOutput {
    #[serde(rename = "PickupPendingStatusResponse")]
    response: PendingResponse,
}
#[derive(Deserialize)]
struct CancelResponse {
    #[serde(rename = "Response")]
    response: ResponseStatus,
}
#[derive(Deserialize)]
struct CancelOutput {
    #[serde(rename = "PickupCancelResponse")]
    response: CancelResponse,
}

fn check(plan: &ResolvedPlan) -> Result<()> {
    plan.site.details.validate()?;
    if plan.carrier != PickupCarrier::Ups
        || plan
            .parcels
            .iter()
            .any(|p| p.service.carrier() != PickupCarrier::Ups)
    {
        return Err(Error::new(ErrorCode::InvalidInput));
    }
    Ok(())
}
fn date_info(plan: &ResolvedPlan, date: &str) -> DateInfo {
    DateInfo {
        pickup_date: date.replace('-', ""),
        ready_time: plan.ready_time.replace(':', ""),
        close_time: plan.close_time.replace(':', ""),
    }
}
fn service(service: PickupService) -> Result<&'static str> {
    match service {
        PickupService::UpsGround => Ok("003"),
        PickupService::UpsNextDayAir => Ok("001"),
        PickupService::UpsSecondDayAir => Ok("002"),
        PickupService::UpsThreeDaySelect => Ok("012"),
        PickupService::UpsWorldwideExpress => Ok("007"),
        PickupService::UpsWorldwideExpedited => Ok("008"),
        _ => Err(Error::new(ErrorCode::InvalidInput)),
    }
}
fn safe_number(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 100
        && value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-')
}

impl Pickup for Ups {
    fn carrier(&self) -> PickupCarrier {
        PickupCarrier::Ups
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            lookup: Lookup::Pending,
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
            check(plan)?;
            let zone = jiff::tz::TimeZoneDatabase::bundled()
                .get(&plan.site.details.timezone)
                .map_err(|_| Error::new(ErrorCode::InvalidInput))?;
            let today = jiff::Timestamp::now().to_zoned(zone).date();
            let start = if let Some(date) = &plan.date {
                validate_date(date)?;
                date.parse::<jiff::civil::Date>()
                    .map_err(|_| Error::new(ErrorCode::InvalidInput))?
            } else {
                today
            };
            let address = &plan.site.details.address;
            for days in 0..if plan.date.is_some() { 1 } else { 8 } {
                let date = start
                    .checked_add(jiff::Span::new().days(days))
                    .map_err(|_| Error::new(ErrorCode::InvalidInput))?
                    .to_string();
                let body = RateEnvelope {
                    request: RateRequest {
                        request: Request {},
                        shipper_account: Account {
                            account_number: &self.account,
                            account_country_code: "US",
                        },
                        pickup_address: RateAddress {
                            city: &address.city,
                            state_province: address.state.as_str(),
                            postal_code: &address.postal_code,
                            country_code: &address.country,
                            residential_indicator: if plan.site.details.residential {
                                "Y"
                            } else {
                                "N"
                            },
                        },
                        alternate_address_indicator: "Y",
                        service_date_option: "03",
                        pickup_date_info: date_info(plan, &date),
                        tax_information_indicator: "Y",
                    },
                };
                let response = self
                    .http
                    .request(
                        Method::POST,
                        "/api/shipments/v2409/pickup/oncall",
                        &[],
                        Some(&body),
                        &[],
                        false,
                    )
                    .await;
                let response = match response {
                    Err(error) if error.code == ErrorCode::Ineligible && plan.date.is_none() => {
                        continue;
                    }
                    other => other?,
                };
                let output: RateOutput = response.json(false)?;
                if output.response.response.status.code != "1" {
                    return Err(Error::new(ErrorCode::Unavailable));
                }
                let rate = output.response.rate;
                let fee = if let Some(amount) = rate.discounted_total.or(rate.total) {
                    if rate.currency.len() != 3
                        || !rate.currency.bytes().all(|c| c.is_ascii_uppercase())
                        || !valid_amount(&amount)
                    {
                        return Err(Error::new(ErrorCode::Unavailable));
                    }
                    Fee::Estimate {
                        currency: rate.currency,
                        amount,
                    }
                } else {
                    Fee::Unquoted
                };
                return Ok(Offer {
                    date,
                    ready_time: plan.ready_time.clone(),
                    close_time: plan.close_time.clone(),
                    fee,
                });
            }
            Err(Error::new(ErrorCode::Ineligible))
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
            let site = &plan.site.details;
            let address = &site.address;
            let mut lines = vec![address.street.as_str()];
            if let Some(secondary) = &address.secondary {
                lines.push(secondary);
            }
            let pieces = plan
                .parcels
                .iter()
                .map(|p| {
                    Ok(Piece {
                        service_code: service(p.service)?,
                        quantity: p.count.to_string(),
                        destination_country_code: p.destination_country.as_str(),
                        container_code: "01",
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            let body = CreateEnvelope {
                request: Create {
                    request: Request {},
                    rate_pickup_indicator: "Y",
                    shipper: Shipper {
                        account: Account {
                            account_number: &self.account,
                            account_country_code: "US",
                        },
                    },
                    pickup_date_info: date_info(plan, date),
                    pickup_address: PickupAddress {
                        company_name: site.company.as_deref().unwrap_or(&site.contact_name),
                        contact_name: &site.contact_name,
                        address_line: lines,
                        city: &address.city,
                        state_province: address.state.as_str(),
                        postal_code: &address.postal_code,
                        country_code: &address.country,
                        residential_indicator: if plan.site.details.residential {
                            "Y"
                        } else {
                            "N"
                        },
                        pickup_point: &site.pickup_point,
                        phone: Phone {
                            number: site.phone.chars().filter(char::is_ascii_digit).collect(),
                        },
                    },
                    alternate_address_indicator: "Y",
                    pickup_piece: pieces,
                    total_weight: Weight {
                        weight: weight_string(plan.total_weight_hundredths),
                        unit_of_measurement: "LBS",
                    },
                    payment_method: "01",
                    overweight_indicator: plan.overweight.then_some("Y"),
                    special_instruction: site.instructions.as_deref(),
                    reference_number: plan.reference.simple().to_string(),
                    shipping_labels_available: "Y",
                },
            };
            let response = self
                .http
                .request(
                    Method::POST,
                    "/api/pickupcreation/v2409/pickup",
                    &[],
                    Some(&body),
                    &[],
                    true,
                )
                .await?;
            let output: CreateOutput = response.json(true)?;
            let number = output
                .response
                .prn
                .filter(|s| safe_number(s))
                .ok_or_else(|| Error::new(ErrorCode::UnknownOutcome))?;
            if output.response.response.status.code != "1" {
                return Err(Error::new(ErrorCode::UnknownOutcome));
            }
            Ok(Confirmation {
                number,
                pickup_date: date.into(),
                location_code: None,
                etag: None,
            })
        })
    }
    fn lookup<'a>(
        &'a self,
        plan: &'a ResolvedPlan,
        saved: &'a Confirmation,
    ) -> PickupFuture<'a, Confirmation> {
        Box::pin(async move {
            let response = self
                .http
                .request::<()>(
                    Method::GET,
                    "/api/shipments/v2409/pickup/oncall",
                    &[],
                    None,
                    &[("AccountNumber", &self.account)],
                    false,
                )
                .await?;
            let output: PendingOutput = response.json(false)?;
            if output.response.response.status.code != "1" {
                return Err(Error::new(ErrorCode::Unavailable));
            }
            let mut rows = output.response.pending.into_iter().filter(|p| {
                p.prn == saved.number
                    && p.date == saved.pickup_date.replace('-', "")
                    && p.reference
                        .as_deref()
                        .is_none_or(|r| r == plan.reference.simple().to_string())
            });
            let Some(row) = rows.next() else {
                return Err(Error::new(ErrorCode::NotFound));
            };
            if rows.next().is_some() || !safe_number(&row.prn) {
                return Err(Error::new(ErrorCode::Conflict));
            }
            Ok(saved.clone())
        })
    }
    fn cancel<'a>(
        &'a self,
        _plan: &'a ResolvedPlan,
        saved: &'a Confirmation,
    ) -> PickupFuture<'a, ()> {
        Box::pin(async move {
            if !safe_number(&saved.number) {
                return Err(Error::new(ErrorCode::InvalidInput));
            }
            let response = self
                .http
                .request::<()>(
                    Method::DELETE,
                    "/api/shipments/v2409/pickup/02",
                    &[],
                    None,
                    &[("Prn", &saved.number)],
                    true,
                )
                .await?;
            let output: CancelOutput = response.json(true)?;
            if output.response.response.status.code != "1" {
                return Err(Error::new(ErrorCode::UnknownOutcome));
            }
            Ok(())
        })
    }
}

fn valid_amount(value: &str) -> bool {
    let mut split = value.split('.');
    let whole = split.next().unwrap_or("");
    let fraction = split.next();
    !whole.is_empty()
        && whole.len() <= 12
        && whole.bytes().all(|c| c.is_ascii_digit())
        && fraction
            .is_none_or(|s| !s.is_empty() && s.len() <= 2 && s.bytes().all(|c| c.is_ascii_digit()))
        && split.next().is_none()
}
