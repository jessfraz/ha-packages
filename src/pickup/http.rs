use std::{sync::Arc, time::Duration};

use reqwest::{Client, Method, StatusCode, header::HeaderMap};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};
use tokio::{sync::Mutex, time::Instant};

use crate::pickup::{Credentials, Environment, Error, ErrorCode, Result, models::PickupCarrier};

const MAX_RESPONSE: usize = 2 * 1024 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(25);

#[derive(Clone)]
pub(crate) struct CarrierHttp {
    carrier: PickupCarrier,
    client: Client,
    client_id: String,
    client_secret: String,
    origin: String,
    token_path: &'static str,
    state: Arc<Mutex<State>>,
    fingerprint: [u8; 32],
}

struct State {
    token: Option<Token>,
    not_before: Option<Instant>,
}

struct Token {
    value: String,
    expires: Instant,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(deserialize_with = "seconds")]
    expires_in: u64,
}

fn seconds<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<u64, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Seconds {
        Number(u64),
        Text(String),
    }
    match Seconds::deserialize(deserializer)? {
        Seconds::Number(value) => Ok(value),
        Seconds::Text(value) => value.parse().map_err(serde::de::Error::custom),
    }
}

impl CarrierHttp {
    pub fn new(
        carrier: PickupCarrier,
        credentials: Credentials,
        account: Option<&str>,
    ) -> Result<Self> {
        for value in [&credentials.client_id, &credentials.client_secret] {
            if value.is_empty() || value.len() > 8192 || value.chars().any(char::is_control) {
                return Err(Error::new(ErrorCode::InvalidInput));
            }
        }
        if account.is_some_and(|value| {
            value.is_empty()
                || value.len() > 64
                || !value.bytes().all(|b| b.is_ascii_alphanumeric())
        }) {
            return Err(Error::new(ErrorCode::InvalidInput));
        }
        let sandbox = matches!(credentials.environment, Environment::Sandbox);
        let origin = match (carrier, sandbox) {
            (PickupCarrier::Usps, false) => "https://apis.usps.com",
            (PickupCarrier::Usps, true) => "https://apis-tem.usps.com",
            (PickupCarrier::Ups, false) => "https://onlinetools.ups.com",
            (PickupCarrier::Ups, true) => "https://wwwcie.ups.com",
            (PickupCarrier::FedEx, false) => "https://apis.fedex.com",
            (PickupCarrier::FedEx, true) => "https://apis-sandbox.fedex.com",
        };
        let token_path = match carrier {
            PickupCarrier::Usps => "/oauth2/v3/token",
            PickupCarrier::Ups => "/security/v1/oauth/token",
            PickupCarrier::FedEx => "/oauth/token",
        };
        let mut digest = Sha256::new();
        for value in [
            carrier.as_str(),
            origin,
            &credentials.client_id,
            account.unwrap_or(""),
        ] {
            digest.update(value.as_bytes());
            digest.update([0]);
        }
        Ok(Self {
            carrier,
            client: Client::builder()
                .timeout(REQUEST_TIMEOUT)
                .connect_timeout(Duration::from_secs(8))
                .redirect(reqwest::redirect::Policy::none())
                .retry(reqwest::retry::never())
                .build()
                .map_err(|_| Error::new(ErrorCode::Unavailable))?,
            client_id: credentials.client_id,
            client_secret: credentials.client_secret,
            origin: origin.into(),
            token_path,
            state: Arc::new(Mutex::new(State {
                token: None,
                not_before: Some(Instant::now()),
            })),
            fingerprint: digest.finalize().into(),
        })
    }

    #[cfg(test)]
    pub(crate) fn with_origin(mut self, origin: String) -> Self {
        self.origin = origin;
        self
    }

    pub fn fingerprint(&self) -> [u8; 32] {
        self.fingerprint
    }

    async fn token(&self, state: &mut State) -> Result<String> {
        if let Some(token) = &state.token
            && token.expires > Instant::now()
        {
            return Ok(token.value.clone());
        }
        #[derive(Serialize)]
        struct OAuth<'a> {
            grant_type: &'static str,
            client_id: &'a str,
            client_secret: &'a str,
        }
        let body = OAuth {
            grant_type: "client_credentials",
            client_id: &self.client_id,
            client_secret: &self.client_secret,
        };
        let request = self
            .client
            .post(format!("{}{}", self.origin, self.token_path));
        let request = match self.carrier {
            PickupCarrier::Usps => request.json(&body),
            PickupCarrier::Ups => request
                .basic_auth(&self.client_id, Some(&self.client_secret))
                .form(&[("grant_type", "client_credentials")]),
            PickupCarrier::FedEx => request.form(&body),
        };
        let response = request
            .send()
            .await
            .map_err(|_| Error::new(ErrorCode::Unavailable))?;
        let response = read(response, state, false).await?;
        let token: TokenResponse = serde_json::from_slice(&response.body)
            .map_err(|_| Error::new(ErrorCode::Unavailable))?;
        if token.access_token.is_empty()
            || token.access_token.len() > 16384
            || token.access_token.chars().any(char::is_control)
            || token.expires_in < 5
        {
            return Err(Error::new(ErrorCode::Unavailable));
        }
        state.token = Some(Token {
            value: token.access_token.clone(),
            expires: Instant::now()
                + Duration::from_secs(token.expires_in.min(86400).saturating_sub(5)),
        });
        Ok(token.access_token)
    }

    pub async fn request<B: Serialize + ?Sized>(
        &self,
        method: Method,
        path: &str,
        query: &[(String, String)],
        body: Option<&B>,
        headers: &[(&str, &str)],
        mutation: bool,
    ) -> Result<Response> {
        // One shared gate owns OAuth, pacing and the actual call. A manual
        // request cannot jump around a carrier Retry-After deadline.
        let mut state = tokio::time::timeout(Duration::from_secs(5), self.state.lock())
            .await
            .map_err(|_| Error::new(ErrorCode::Unavailable))?;
        let now = Instant::now();
        match state.not_before {
            None => {
                return Err(Error {
                    code: ErrorCode::RateLimited,
                    retry_after_seconds: Some(u64::MAX),
                });
            }
            Some(deadline) if deadline > now => {
                return Err(Error {
                    code: ErrorCode::RateLimited,
                    retry_after_seconds: Some((deadline - now).as_secs().saturating_add(1)),
                });
            }
            _ => {}
        }
        let token = self.token(&mut state).await?;
        let mut request = self
            .client
            .request(method, format!("{}{}", self.origin, path))
            .bearer_auth(token)
            .query(query);
        if let Some(body) = body {
            request = request.json(body);
        }
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        let response = request.send().await.map_err(|_| {
            Error::new(if mutation {
                ErrorCode::UnknownOutcome
            } else {
                ErrorCode::Unavailable
            })
        })?;
        if response.status() == StatusCode::UNAUTHORIZED {
            state.token = None;
        }
        read(response, &mut state, mutation).await
    }
}

pub(crate) struct Response {
    pub body: Vec<u8>,
    pub headers: HeaderMap,
}
impl Response {
    pub fn json<T: DeserializeOwned>(&self, mutation: bool) -> Result<T> {
        serde_json::from_slice(&self.body).map_err(|_| {
            Error::new(if mutation {
                ErrorCode::UnknownOutcome
            } else {
                ErrorCode::Unavailable
            })
        })
    }
}

async fn read(
    mut response: reqwest::Response,
    state: &mut State,
    mutation: bool,
) -> Result<Response> {
    let status = response.status();
    let headers = response.headers().clone();
    let retry_after = headers
        .get("retry-after")
        .and_then(|h| h.to_str().ok())
        .and_then(|v| {
            v.parse::<u64>().ok().or_else(|| {
                jiff::Timestamp::strptime("%a, %d %b %Y %H:%M:%S GMT", v)
                    .ok()
                    .map(|date| {
                        date.as_second()
                            .saturating_sub(jiff::Timestamp::now().as_second())
                            .max(1)
                            .unsigned_abs()
                    })
            })
        })
        .map(|n| n.max(1));
    if let Some(seconds) = retry_after {
        state.not_before = Instant::now().checked_add(Duration::from_secs(seconds));
    }
    if !status.is_success() {
        let code = match status.as_u16() {
            401 | 403 => ErrorCode::Unauthorized,
            404 => ErrorCode::NotFound,
            409 | 412 => ErrorCode::Conflict,
            429 => ErrorCode::RateLimited,
            400 | 422 => ErrorCode::Ineligible,
            _ if mutation => ErrorCode::UnknownOutcome,
            _ => ErrorCode::Unavailable,
        };
        if status == StatusCode::TOO_MANY_REQUESTS && retry_after.is_none() {
            state.not_before = Instant::now().checked_add(Duration::from_secs(60));
        }
        return Err(Error {
            code,
            retry_after_seconds: retry_after
                .or_else(|| (status == StatusCode::TOO_MANY_REQUESTS).then_some(60)),
        });
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| {
        Error::new(if mutation {
            ErrorCode::UnknownOutcome
        } else {
            ErrorCode::Unavailable
        })
    })? {
        if body.len() + chunk.len() > MAX_RESPONSE {
            return Err(Error::new(if mutation {
                ErrorCode::UnknownOutcome
            } else {
                ErrorCode::Unavailable
            }));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(Response { body, headers })
}
