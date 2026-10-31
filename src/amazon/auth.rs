//! Amazon device sign-in, adapted from alexapy (Apache-2.0) and ha-amazon.
//! See NOTICE and LICENSE-Apache-2.0 for attribution and terms.

use base64::{
    Engine as _,
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
};
use reqwest::{Method, Url};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{
    amazon::{AmazonClient, provider_error},
    shipping_api::error::{Error, ErrorKind},
};

const APP: &str = "Alexa Media Player";
const VERSION: &str = "2.2.556530.0";
const DEVICE_SUFFIX: &str = "23413249564c5635564d32573831";
const LANDING: &str = "https://www.amazon.com/ap/maplanding";

pub(crate) fn verifier() -> Result<String, Error> {
    let mut bytes = [0; 32];
    getrandom::fill(&mut bytes).map_err(|_| provider_error(ErrorKind::Configuration))?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

fn device_id(serial: &str) -> String {
    let mut value = String::with_capacity(serial.len() * 2 + DEVICE_SUFFIX.len());
    for byte in serial.bytes() {
        use std::fmt::Write;
        let _ = write!(value, "{byte:02x}");
    }
    value.push_str(DEVICE_SUFFIX);
    value
}

pub(crate) fn sign_in(serial: &str, verifier: &str) -> Result<String, Error> {
    let mut url = Url::parse("https://www.amazon.com/ap/signin")
        .map_err(|_| provider_error(ErrorKind::Configuration))?;
    url.query_pairs_mut().extend_pairs([
        ("openid.return_to", LANDING),
        ("openid.assoc_handle", "amzn_dp_project_dee_ios"),
        (
            "openid.identity",
            "http://specs.openid.net/auth/2.0/identifier_select",
        ),
        ("pageId", "amzn_dp_project_dee_ios"),
        ("accountStatusPolicy", "P1"),
        (
            "openid.claimed_id",
            "http://specs.openid.net/auth/2.0/identifier_select",
        ),
        ("openid.mode", "checkid_setup"),
        ("openid.ns.oa2", "http://www.amazon.com/ap/ext/oauth/2"),
        (
            "openid.oa2.client_id",
            &format!("device:{}", device_id(serial)),
        ),
        (
            "openid.ns.pape",
            "http://specs.openid.net/extensions/pape/1.0",
        ),
        ("openid.oa2.response_type", "code"),
        ("openid.ns", "http://specs.openid.net/auth/2.0"),
        ("openid.pape.max_auth_age", "0"),
        ("openid.oa2.scope", "device_auth_access offline_access"),
        ("openid.oa2.code_challenge_method", "S256"),
        (
            "openid.oa2.code_challenge",
            &URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes())),
        ),
        ("language", "en_US"),
    ]);
    Ok(url.into())
}

pub(crate) fn code(input: &str) -> Result<String, Error> {
    if input.len() > 8192 || input.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return Err(provider_error(ErrorKind::InvalidResponse));
    }
    let url = Url::parse(input).map_err(|_| provider_error(ErrorKind::InvalidResponse))?;
    if url.scheme() != "https"
        || url.host_str() != Some("www.amazon.com")
        || url.path() != "/ap/maplanding"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some_and(|port| port != 443)
        || url.fragment().is_some()
    {
        return Err(provider_error(ErrorKind::InvalidResponse));
    }
    let values = url
        .query_pairs()
        .filter(|(key, _)| key == "openid.oa2.authorization_code")
        .map(|(_, value)| value.into_owned())
        .collect::<Vec<_>>();
    if values.len() != 1
        || values[0].is_empty()
        || values[0].len() > 4096
        || values[0]
            .chars()
            .any(|c| c.is_control() || c.is_whitespace())
    {
        return Err(provider_error(ErrorKind::InvalidResponse));
    }
    values
        .into_iter()
        .next()
        .ok_or_else(|| provider_error(ErrorKind::InvalidResponse))
}

#[derive(Serialize)]
struct Register<'a> {
    requested_extensions: [&'static str; 2],
    cookies: CookieRequest,
    registration_data: Device<'a>,
    auth_data: Authorization<'a>,
    user_context_map: Context,
    requested_token_type: [&'static str; 3],
}
#[derive(Serialize)]
struct CookieRequest {
    website_cookies: [(); 0],
    domain: &'static str,
}
#[derive(Serialize)]
struct Device<'a> {
    domain: &'static str,
    app_version: &'static str,
    device_type: &'static str,
    device_name: &'static str,
    os_version: &'static str,
    device_serial: &'a str,
    device_model: &'static str,
    app_name: &'static str,
    software_version: &'static str,
}
#[derive(Serialize)]
struct Authorization<'a> {
    client_id: String,
    authorization_code: &'a str,
    code_verifier: &'a str,
    code_algorithm: &'static str,
    client_domain: &'static str,
}
#[derive(Serialize)]
struct Context {
    frc: String,
}
#[derive(Deserialize)]
struct RegisterBody {
    response: RegisterResponse,
}
#[derive(Deserialize)]
struct RegisterResponse {
    success: RegisterSuccess,
}
#[derive(Deserialize)]
struct RegisterSuccess {
    tokens: RegisterTokens,
    extensions: RegisterExtensions,
}
#[derive(Deserialize)]
struct RegisterTokens {
    bearer: Bearer,
}
#[derive(Deserialize)]
struct Bearer {
    refresh_token: String,
}
#[derive(Deserialize)]
struct RegisterExtensions {
    customer_info: Customer,
}
#[derive(Deserialize)]
struct Customer {
    user_id: String,
}

pub(crate) struct Registration {
    pub token: String,
    pub customer: String,
}

pub(crate) async fn register(
    client: &AmazonClient,
    serial: &str,
    verifier: &str,
    code: &str,
) -> Result<Registration, Error> {
    let mut bytes = [0; 313];
    getrandom::fill(&mut bytes).map_err(|_| provider_error(ErrorKind::Configuration))?;
    let body = Register {
        requested_extensions: ["device_info", "customer_info"],
        cookies: CookieRequest {
            website_cookies: [],
            domain: ".amazon.com",
        },
        registration_data: Device {
            domain: "Device",
            app_version: VERSION,
            device_type: "A2IVLV5VM2W81",
            device_name: "%FIRST_NAME%'s%DUPE_STRATEGY_1ST%Home Assistant Shipments",
            os_version: "16.6",
            device_serial: serial,
            device_model: "iPhone",
            app_name: APP,
            software_version: "1",
        },
        auth_data: Authorization {
            client_id: device_id(serial),
            authorization_code: code,
            code_verifier: verifier,
            code_algorithm: "SHA-256",
            client_domain: "DeviceLegacy",
        },
        user_context_map: Context {
            frc: STANDARD.encode(bytes).trim_end_matches('=').into(),
        },
        requested_token_type: ["bearer", "mac_dms", "website_cookies"],
    };
    let response: RegisterBody = client
        .json(
            client
                .request(Method::POST, "https://api.amazon.com/auth/register")?
                .json(&body),
        )
        .await?;
    let registration = Registration {
        token: response.response.success.tokens.bearer.refresh_token,
        customer: response.response.success.extensions.customer_info.user_id,
    };
    if registration.token.is_empty()
        || registration.token.len() > 8192
        || registration.customer.is_empty()
        || registration.customer.len() > 256
    {
        return Err(provider_error(ErrorKind::InvalidResponse));
    }
    Ok(registration)
}

#[derive(Serialize)]
struct Token<'a> {
    app_name: &'static str,
    app_version: &'static str,
    #[serde(rename = "di.sdk.version")]
    sdk: &'static str,
    package_name: &'static str,
    #[serde(rename = "di.hw.version")]
    hardware: &'static str,
    platform: &'static str,
    #[serde(rename = "di.os.name")]
    os: &'static str,
    #[serde(rename = "di.os.version")]
    os_version: &'static str,
    current_version: &'static str,
    previous_version: &'static str,
    source_token: &'a str,
    requested_token_type: &'a str,
    source_token_type: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    domain: Option<&'static str>,
}
impl<'a> Token<'a> {
    fn new(token: &'a str, kind: &'a str, domain: Option<&'static str>) -> Self {
        Self {
            app_name: APP,
            app_version: VERSION,
            sdk: "6.12.4",
            package_name: "com.amazon.echo",
            hardware: "iPhone",
            platform: "iOS",
            os: "iOS",
            os_version: "16.6",
            current_version: "6.12.4",
            previous_version: "6.12.4",
            source_token: token,
            requested_token_type: kind,
            source_token_type: "refresh_token",
            domain,
        }
    }
}
#[derive(Deserialize)]
struct Access {
    access_token: String,
}
#[derive(Deserialize)]
struct CookieBody {
    response: CookieResponse,
}
#[derive(Deserialize)]
struct CookieResponse {
    tokens: CookieTokens,
}
#[derive(Deserialize)]
struct CookieTokens {
    cookies: std::collections::BTreeMap<String, Vec<WebsiteCookie>>,
}
#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct WebsiteCookie {
    name: String,
    value: String,
    #[serde(default)]
    path: Option<String>,
}

pub(crate) async fn authenticate(client: &AmazonClient, token: &str) -> Result<(), Error> {
    let access: Access = client
        .json(
            client
                .request(Method::POST, "https://api.amazon.com/auth/token")?
                .form(&Token::new(token, "access_token", None)),
        )
        .await?;
    if access.access_token.is_empty() {
        return Err(provider_error(ErrorKind::InvalidResponse));
    }
    let response: CookieBody = client
        .json(
            client
                .request(
                    Method::POST,
                    "https://www.amazon.com/ap/exchangetoken/cookies",
                )?
                .form(&Token::new(token, "auth_cookies", Some(".amazon.com"))),
        )
        .await?;
    let origin = Url::parse(crate::amazon::pages::ORIGIN)
        .map_err(|_| provider_error(ErrorKind::Configuration))?;
    let jar = reqwest::cookie::Jar::default();
    let mut count = 0;
    for (domain, cookies) in response.response.tokens.cookies {
        if ![
            ".amazon.com",
            "amazon.com",
            ".www.amazon.com",
            "www.amazon.com",
        ]
        .contains(&domain.as_str())
        {
            continue;
        }
        for cookie in cookies {
            let value = cookie.value.trim_matches('"');
            if cookie.name.is_empty()
                || cookie.name.len() > 128
                || value.len() > 8192
                || !cookie
                    .name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
                || value
                    .bytes()
                    .any(|b| b.is_ascii_control() || matches!(b, b';' | b','))
                || cookie.path.as_deref().is_some_and(|p| p != "/")
            {
                return Err(provider_error(ErrorKind::InvalidResponse));
            }
            jar.add_cookie_str(
                &format!(
                    "{}={value}; Domain={domain}; Path=/; Secure; HttpOnly",
                    cookie.name
                ),
                &origin,
            );
            count += 1;
        }
    }
    if count == 0 {
        return Err(provider_error(ErrorKind::InvalidResponse));
    }
    client.replace_cookies(jar)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::amazon::auth;
    #[test]
    fn callback_is_fixed_origin_path_and_one_code() {
        assert_eq!(
            auth::code(
                "https://www.amazon.com/ap/maplanding?openid.oa2.authorization_code=TEST-CODE"
            )
            .unwrap(),
            "TEST-CODE"
        );
        for value in [
            "http://www.amazon.com/ap/maplanding?openid.oa2.authorization_code=x",
            "https://www.amazon.com.evil.example/ap/maplanding?openid.oa2.authorization_code=x",
            "https://user@www.amazon.com/ap/maplanding?openid.oa2.authorization_code=x",
            "https://www.amazon.com/ap/maplanding?openid.oa2.authorization_code=x&openid.oa2.authorization_code=y",
            "https://www.amazon.com/ap/maplanding?openid.oa2.authorization_code=x#fragment",
            "https://www.amazon.com/other?openid.oa2.authorization_code=x",
        ] {
            assert!(auth::code(value).is_err());
        }
    }
    #[test]
    fn pkce_is_s256_and_no_verifier_is_exposed() {
        let verifier = auth::verifier().unwrap();
        let url = auth::sign_in("SYNTHETICSERIAL", &verifier).unwrap();
        let url = reqwest::Url::parse(&url).unwrap();
        let values = url
            .query_pairs()
            .collect::<std::collections::BTreeMap<_, _>>();
        assert_eq!(values["openid.oa2.code_challenge_method"], "S256");
        assert!(!url.as_str().contains(&verifier));
        assert_eq!(
            values["openid.return_to"],
            "https://www.amazon.com/ap/maplanding"
        );
    }
}
