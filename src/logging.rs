//! Operational logs expose a fixed vocabulary, never raw request or error data.

use std::{fmt, io::Write, sync::Mutex};

use serde::Serialize;
use slog::{Drain, KV};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use uuid::Uuid;

pub fn logger() -> slog::Logger {
    with_writer(std::io::stderr())
}

pub fn with_writer(writer: impl Write + Send + 'static) -> slog::Logger {
    slog::Logger::root(
        JsonLog {
            writer: Mutex::new(writer),
        },
        slog::o!(),
    )
}

struct JsonLog<W> {
    writer: Mutex<W>,
}

#[derive(Default, Serialize)]
struct Fields {
    #[serde(skip_serializing_if = "Option::is_none")]
    provider: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    request_id: Option<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    shipment_id: Option<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    method: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    route: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    response_code: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    latency_us: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    operation: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    outcome: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error_code: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    provider_status: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    response_shape: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    page_kind: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    page_fingerprint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    provider_error_code: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    provider_request_id: Option<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    retry_after_seconds: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    duration_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    verified_parcels: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    deferred_link: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    deferred_identity: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    deferred_product: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    deferred_tracking: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    deferred_limit: Option<u64>,
}

#[derive(Serialize)]
struct Entry {
    timestamp: String,
    level: &'static str,
    event: &'static str,
    #[serde(flatten)]
    fields: Fields,
}

impl<W: Write + Send> Drain for JsonLog<W> {
    type Ok = ();
    type Err = slog::Never;

    fn log(&self, record: &slog::Record<'_>, values: &slog::OwnedKVList) -> Result<(), Self::Err> {
        // Unknown messages are discarded rather than interpolated. Dropshot's
        // internal errors and URI-bearing diagnostics must not bypass the same
        // privacy rules as application events.
        let Some(event) = record.msg().as_str().and_then(event_name) else {
            return Ok(());
        };
        let mut fields = Fields::default();
        let _ = values.serialize(record, &mut fields);
        let _ = record.kv().serialize(record, &mut fields);
        if event == "request_completed" {
            if fields.method == Some("GET")
                && fields.response_code.is_some_and(|status| status < 400)
            {
                return Ok(());
            }
            fields.error_code = fields.response_code.and_then(response_error);
        } else if event == "request_cancelled" {
            fields.error_code = Some("client_disconnected");
        }
        let Ok(timestamp) = OffsetDateTime::now_utc().format(&Rfc3339) else {
            return Ok(());
        };
        let warning = fields.response_code.is_some_and(|status| status >= 400)
            || fields
                .outcome
                .is_some_and(|outcome| !matches!(outcome, "success" | "saved" | "verified"))
            || matches!(
                event,
                "request_cancelled" | "request_panicked" | "server_accept_error"
            );
        let level = match record.level() {
            slog::Level::Critical => "critical",
            slog::Level::Error => "error",
            slog::Level::Warning => "warn",
            _ if warning => "warn",
            slog::Level::Info => "info",
            slog::Level::Debug => "debug",
            slog::Level::Trace => "trace",
        };
        let Ok(mut line) = serde_json::to_vec(&Entry {
            timestamp,
            level,
            event,
            fields,
        }) else {
            return Ok(());
        };
        line.push(b'\n');
        if let Ok(mut writer) = self.writer.lock() {
            // Logging failure must not interrupt request handling or panic.
            let _ = writer.write_all(&line);
        }
        Ok(())
    }
}

fn event_name(message: &str) -> Option<&'static str> {
    Some(match message {
        "request completed" => "request_completed",
        "request handling cancelled (client disconnected)" => "request_cancelled",
        "request completed after handler was already cancelled" => {
            "request_completed_after_cancellation"
        }
        "handler panicked; propagating panic" => "request_panicked",
        "listening" => "server_started",
        "beginning graceful shutdown" => "server_stopping",
        "accept error" => "server_accept_error",
        "provider_request" => "provider_request",
        "amazon_connection" => "amazon_connection",
        "amazon_preview" => "amazon_preview",
        "tracking_verification" => "tracking_verification",
        "tracking_registration" => "tracking_registration",
        "tracking_sync" => "tracking_sync",
        "integration_discovery" => "integration_discovery",
        _ => return None,
    })
}

impl slog::Serializer for Fields {
    fn emit_arguments(&mut self, key: slog::Key, value: &fmt::Arguments<'_>) -> slog::Result {
        // Check the key before formatting it: ignored values can contain
        // secrets, addresses, body fragments, or provider error responses.
        match key.as_str() {
            "provider" => {
                self.provider = allowed(
                    &value.to_string(),
                    &["easypost", "aftership", "doordash", "amazon_account"],
                )
            }
            "req_id" | "request_id" => self.request_id = Uuid::parse_str(&value.to_string()).ok(),
            "shipment_id" => self.shipment_id = Uuid::parse_str(&value.to_string()).ok(),
            "method" => {
                self.method = allowed(
                    &value.to_string(),
                    &["GET", "POST", "PATCH", "PUT", "DELETE", "HEAD", "OPTIONS"],
                )
            }
            "uri" => self.route = Some(route_template(&value.to_string())),
            "response_code" => self.response_code = http_status(&value.to_string()),
            "provider_status" => self.provider_status = http_status(&value.to_string()),
            "verified_parcels" => self.verified_parcels = bounded_number(&value.to_string(), 500),
            "deferred_link" => self.deferred_link = bounded_number(&value.to_string(), 500),
            "deferred_identity" => self.deferred_identity = bounded_number(&value.to_string(), 500),
            "deferred_product" => self.deferred_product = bounded_number(&value.to_string(), 500),
            "deferred_tracking" => self.deferred_tracking = bounded_number(&value.to_string(), 500),
            "deferred_limit" => self.deferred_limit = bounded_number(&value.to_string(), 500),
            "page_kind" => {
                self.page_kind = allowed(
                    &value.to_string(),
                    &["orders_listing", "item_details", "tracking_page"],
                )
            }
            "page_fingerprint" => {
                let value = value.to_string();
                if value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                    self.page_fingerprint = Some(value);
                }
            }
            "response_shape" => {
                self.response_shape = allowed(
                    &value.to_string(),
                    &[
                        "challenge",
                        "sign_in",
                        "business_shell",
                        "unrecognized_html",
                        "encoded_gzip",
                        "encoded_brotli",
                        "encoded_deflate",
                        "encoded_other",
                        "invalid_utf8",
                        "unclassified_error_page",
                        "amazon_service_error_page",
                        "tracking_page_state_missing",
                        "tracking_page_state_multiple",
                        "tracking_page_state_invalid_json",
                        "tracking_page_state_invalid_shape",
                        "tracking_package_id_numeric",
                        "tracking_package_index_numeric",
                        "tracking_carrier_missing",
                        "tracking_number_missing",
                        "tracking_status_unsupported",
                        "tracking_identity_mismatch",
                        "order_details_listing_page",
                        "order_details_tracking_links_missing",
                        "order_details_tracking_identity_missing",
                        "order_details_tracking_order_mismatch",
                        "order_details_identity_mismatch",
                    ],
                )
            }
            "provider_request_id" => {
                self.provider_request_id = Uuid::parse_str(&value.to_string()).ok()
            }
            "provider_error_code" => {
                self.provider_error_code = allowed(
                    &value.to_string(),
                    &[
                        "CREDENTIALS_NOT_FOUND",
                        "TRACKER.NO_CARRIER_FOR_CODE",
                        "TRACKER.CARRIER_CODE_MISMATCH",
                        "TRACKER.MULTIPLE_CARRIERS_FOR_CODE",
                        "TRACKER.INVALID_PARAMS",
                        "TRACKER.INVALID_TEST_CODE",
                        "TRACKER.NO_CARRIER_ACCOUNT",
                        "Pending_002",
                        "Pending_004",
                        "Pending_006",
                        "4005",
                        "4007",
                        "4008",
                        "4009",
                        "4010",
                        "4011",
                        "4012",
                        "4017",
                    ],
                );
            }
            "latency_us" => self.latency_us = bounded_number(&value.to_string(), 86_400_000_000),
            "duration_ms" => self.duration_ms = bounded_number(&value.to_string(), 86_400_000),
            "retry_after_seconds" => {
                self.retry_after_seconds = bounded_number(&value.to_string(), 21_600)
            }
            "operation" => {
                self.operation = allowed(
                    &value.to_string(),
                    &[
                        "register",
                        "prepare",
                        "reconcile",
                        "retrieve",
                        "health",
                        "create",
                        "update",
                        "sync",
                        "discovery",
                        "callback",
                        "flow",
                        "registration",
                        "access_token",
                        "cookies",
                        "orders",
                        "account",
                        "credentials",
                    ],
                )
            }
            "outcome" => {
                self.outcome = allowed(
                    &value.to_string(),
                    &[
                        "success",
                        "verified",
                        "not_saved",
                        "saved",
                        "rejected",
                        "error",
                        "throttled",
                        "unavailable",
                        "rate_limited",
                    ],
                )
            }
            "error_code" => {
                self.error_code = allowed(
                    &value.to_string(),
                    &[
                        "tracking_required",
                        "tracking_recipient_url_required",
                        "tracking_recipient_url_invalid",
                        "tracking_recipient_url_mismatch",
                        "recipient_page_unavailable",
                        "registration_ambiguous",
                        "registration_expired",
                        "tracking_unsupported",
                        "tracking_invalid",
                        "tracking_carrier_mismatch",
                        "tracking_carrier_ambiguous",
                        "tracking_test_key",
                        "tracking_provider_unconfigured",
                        "tracking_provider_credentials",
                        "tracking_carrier_account_required",
                        "tracking_carrier_credentials_required",
                        "tracking_verification_rate_limited",
                        "tracking_verification_unavailable",
                        "provider_http",
                        "provider_timeout",
                        "local_provider_queue_timeout",
                        "local_provider_queue_full",
                        "provider_connection",
                        "provider_invalid_response",
                        "provider_response_too_large",
                        "provider_configuration",
                        "provider_authentication_required",
                        "provider_throttled",
                        "provider_rate_limited",
                        "database_error",
                        "discovery_unavailable",
                        "invalid_input",
                        "expired_flow",
                        "account_identity_missing",
                        "account_mismatch",
                        "session_changed",
                        "upstream_rejected",
                        "unexpected_response",
                        "timeout",
                        "connection",
                        "rate_limited",
                        "upstream_http",
                        "persistence",
                        "configuration",
                        "unsupported_orders_layout",
                        "challenge_page",
                        "in_progress",
                        "provider_challenge_required",
                    ],
                )
            }
            _ => {}
        }
        Ok(())
    }
}

fn allowed(value: &str, choices: &[&'static str]) -> Option<&'static str> {
    choices
        .iter()
        .copied()
        .find(|candidate| *candidate == value)
}

fn bounded_number(value: &str, maximum: u64) -> Option<u64> {
    value.parse().ok().filter(|number| *number <= maximum)
}

fn http_status(value: &str) -> Option<u16> {
    value
        .parse()
        .ok()
        .filter(|status| (100..=599).contains(status))
}

fn response_error(status: u16) -> Option<&'static str> {
    Some(match status {
        100..=399 => return None,
        400 | 422 => "invalid_request",
        401 => "unauthorized",
        403 => "forbidden",
        404 => "not_found",
        405 => "method_not_allowed",
        409 => "conflict",
        412 => "revision_mismatch",
        413 => "request_too_large",
        415 => "unsupported_media_type",
        429 => "rate_limited",
        503 => "service_unavailable",
        500..=599 => "internal_error",
        _ => "request_failed",
    })
}

fn route_template(uri: &str) -> &'static str {
    if uri.len() > 8192 {
        return "unmatched";
    }
    let Ok(uri) = uri.parse::<http::Uri>() else {
        return "unmatched";
    };
    match uri.path() {
        "/" => "/",
        "/app.js" => "/app.js",
        "/style.css" => "/style.css",
        "/openapi.json" => "/openapi.json",
        "/v1/shipments" => "/v1/shipments",
        "/v1/carriers" => "/v1/carriers",
        "/v1/delivery-estimates" => "/v1/delivery-estimates",
        "/v1/events" => "/v1/events",
        "/v1/summary" => "/v1/summary",
        "/v1/system" => "/v1/system",
        "/v1/installation" => "/v1/installation",
        "/v1/installation/check" => "/v1/installation/check",
        "/v1/integration/presence" => "/v1/integration/presence",
        "/v1/agents" => "/v1/agents",
        "/v1/amazon" => "/v1/amazon",
        "/v1/amazon/sign-in" => "/v1/amazon/sign-in",
        "/v1/amazon/complete" => "/v1/amazon/complete",
        "/v1/amazon/preview" => "/v1/amazon/preview",
        "/v1/amazon/enable" => "/v1/amazon/enable",
        "/v1/amazon/refresh" => "/v1/amazon/refresh",
        "/amazon-settings.js" => "/amazon-settings.js",
        "/api-error.js" => "/api-error.js",
        path => {
            let mut segments = path.split('/');
            match (
                segments.next(),
                segments.next(),
                segments.next(),
                segments.next(),
                segments.next(),
                segments.next(),
            ) {
                (Some(""), Some("v1"), Some("agents"), Some(id), None, None) if !id.is_empty() => {
                    "/v1/agents/{id}"
                }
                (Some(""), Some("v1"), Some("shipments"), Some(id), suffix, None)
                    if !id.is_empty() =>
                {
                    match suffix {
                        None => "/v1/shipments/{id}",
                        Some("sources") => "/v1/shipments/{id}/sources",
                        Some("status-override") => "/v1/shipments/{id}/status-override",
                        Some("archive") => "/v1/shipments/{id}/archive",
                        Some("refresh") => "/v1/shipments/{id}/refresh",
                        Some("amazon") => "/v1/shipments/{id}/amazon",
                        Some("events") => "/v1/shipments/{id}/events",
                        _ => "unmatched",
                    }
                }
                _ => "unmatched",
            }
        }
    }
}
