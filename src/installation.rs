//! Live install checks. A recent authenticated adapter heartbeat establishes
//! that Home Assistant actually loaded the integration and reached this app.

use crate::sync::EasyPostClient;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex;

const PRESENCE_MAX_AGE: Duration = Duration::from_secs(180);

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BackupStatus {
    Complete,
    AppOnly,
    Outdated,
    Missing,
    Unavailable,
    Standalone,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BackupReport {
    pub status: BackupStatus,
    pub last_backup: Option<i64>,
}

#[derive(Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct IntegrationPresence {
    pub version: String,
    pub backup: BackupReport,
    #[serde(default)]
    pub administrator_ids: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CheckStatus {
    Pass,
    Attention,
    Unavailable,
    NotApplicable,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
pub struct Check {
    pub status: CheckStatus,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checked_at: Option<u64>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct InstallationReport {
    pub backend: Check,
    pub integration: Check,
    pub version: Check,
    pub authentication: Check,
    pub easypost: Check,
    pub backup: Check,
    pub last_backup: Option<i64>,
}

pub struct Installation {
    supervised: bool,
    presence: Mutex<Option<(Instant, IntegrationPresence)>>,
    provider: Mutex<Option<(Instant, Check)>>,
    probe: Mutex<()>,
}

impl Installation {
    pub fn new(supervised: bool) -> Self {
        Self {
            supervised,
            presence: Mutex::new(None),
            provider: Mutex::new(None),
            probe: Mutex::new(()),
        }
    }

    pub async fn presence(&self, report: IntegrationPresence) {
        *self.presence.lock().await = Some((Instant::now(), report));
    }

    /// The authenticated adapter supplies the active administrator set. Expiry
    /// denies privileged ingress actions until Home Assistant checks in again.
    pub async fn is_admin(&self, id: &str) -> bool {
        self.presence
            .lock()
            .await
            .as_ref()
            .is_some_and(|(at, presence)| {
                at.elapsed() < PRESENCE_MAX_AGE
                    && presence.administrator_ids.iter().any(|admin| admin == id)
            })
    }

    pub async fn check_provider(&self, provider: Option<&EasyPostClient>) {
        let Some(provider) = provider else {
            return;
        };
        // Serialize outbound probes separately from their completed result.
        // Readers never wait for a provider network request.
        let _probe = self.probe.lock().await;
        if self
            .provider
            .lock()
            .await
            .as_ref()
            .is_some_and(|(at, _)| at.elapsed() < Duration::from_secs(60))
        {
            return;
        }
        let mut result = match provider.check_connection().await {
            Ok(()) => Check::pass(
                "Last check: EasyPost accepted the key and allowed read-only tracking access.",
            ),
            Err(error) => Check {
                status: CheckStatus::Attention,
                message: format!("Last EasyPost check failed: {error}"),
                checked_at: None,
            },
        };
        result.checked_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .ok()
            .map(|elapsed| elapsed.as_secs());
        *self.provider.lock().await = Some((Instant::now(), result));
    }

    pub async fn report(&self, provider_enabled: bool) -> InstallationReport {
        let presence = self.presence.lock().await;
        let recent = presence
            .as_ref()
            .filter(|(at, _)| at.elapsed() < PRESENCE_MAX_AGE);
        let integration = if let Some((_, presence)) = recent {
            Check::pass(&format!(
                "Home Assistant integration {} is loaded and connected.",
                presence.version
            ))
        } else if self.supervised {
            Check::attention(
                "No recent integration check-in. Install Shipments through HACS, restart Home Assistant, then confirm it in Settings → Devices & services. If already paired, check the integration for an authentication or connection error.",
            )
        } else {
            Check {
                status: CheckStatus::NotApplicable,
                message: "Standalone server. The Home Assistant integration is optional.".into(),
                checked_at: None,
            }
        };
        let authentication = if recent.is_some() {
            Check::pass("Home Assistant authenticated successfully with this app.")
        } else if self.supervised {
            Check::attention(
                "Waiting for an authenticated integration check-in. Confirm the discovered Shipments connection in Home Assistant.",
            )
        } else {
            Check::pass("Your API credential was accepted.")
        };
        let version = match recent {
            Some((_, presence)) if presence.version == env!("CARGO_PKG_VERSION") => {
                Check::pass(&format!(
                    "The app and Home Assistant integration both run {}.",
                    env!("CARGO_PKG_VERSION")
                ))
            }
            Some((_, presence)) => Check::attention(&format!(
                "The app runs {} and the Home Assistant integration runs {}. Update both the app and HACS integration to the same release, then restart Home Assistant.",
                env!("CARGO_PKG_VERSION"), presence.version
            )),
            None => Check {
                status: if self.supervised {
                    CheckStatus::Unavailable
                } else {
                    CheckStatus::NotApplicable
                },
                message: "No recent integration check-in. The app and integration versions have not been compared.".into(),
                checked_at: None,
            },
        };
        let easypost = if !provider_enabled {
            Check {
                status: CheckStatus::Attention,
                message: "Configure the EasyPost API key before adding shipments or changing tracking numbers. Existing records remain available.".into(),
                checked_at: None,
            }
        } else {
            let provider = self.provider.lock().await;
            if let Some((_, result)) = provider.as_ref() {
                result.clone()
            } else {
                Check::attention(
                    "EasyPost is configured. Run checks to verify the key and connectivity.",
                )
            }
        };
        let backup = match recent.map(|(_, report)| &report.backup.status) {
            None if !self.supervised => Check { status: CheckStatus::NotApplicable, message: "Standalone server. Stop it before backing up its database and credentials. Home Assistant backup inventory is not available here.".into(), checked_at: None },
            Some(BackupStatus::Complete) => Check::pass("A Home Assistant backup includes this app and the integration configuration. Restore has not been tested."),
            Some(BackupStatus::AppOnly) => Check::attention("A backup includes the app, but not Home Assistant configuration. Back up both to preserve pairing."),
            Some(BackupStatus::Outdated) => Check::attention("The available app backup predates pairing. Create a new backup containing Shipments and Home Assistant configuration to preserve this connection."),
            Some(BackupStatus::Missing) => Check::attention("No backup includes this app. Create a Home Assistant backup containing Shipments and Home Assistant configuration."),
            Some(BackupStatus::Standalone) => Check { status: CheckStatus::NotApplicable, message: "Standalone backend. Stop it before backing up its data and credentials; back up Home Assistant configuration separately.".into(), checked_at: None },
            _ => Check { status: CheckStatus::Unavailable, message: "Backup status is unavailable. Check Settings → System → Backups and include Shipments and Home Assistant configuration.".into(), checked_at: None },
        };
        InstallationReport {
            backend: Check::pass("The app is running and SQLite is responding."),
            integration,
            version,
            authentication,
            easypost,
            backup,
            last_backup: recent.and_then(|(_, report)| report.backup.last_backup),
        }
    }
}

impl Check {
    fn pass(message: &str) -> Self {
        Self {
            status: CheckStatus::Pass,
            message: message.into(),
            checked_at: None,
        }
    }
    fn attention(message: &str) -> Self {
        Self {
            status: CheckStatus::Attention,
            message: message.into(),
            checked_at: None,
        }
    }
}

pub fn valid_presence(presence: &IntegrationPresence) -> bool {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_secs())
        .unwrap_or_default();
    let mut administrator_ids = HashSet::new();
    semver::Version::parse(&presence.version).is_ok()
        && presence.version.len() <= 64
        && presence.administrator_ids.len() <= 200
        && presence.administrator_ids.iter().all(|id| {
            !id.is_empty()
                && id.len() <= 200
                && id.bytes().all(|byte| byte.is_ascii_graphic())
                && administrator_ids.insert(id)
        })
        && presence
            .backup
            .last_backup
            .is_none_or(|value| value > 0 && value as u64 <= now + 300)
        && match presence.backup.status {
            BackupStatus::Complete | BackupStatus::AppOnly | BackupStatus::Outdated => {
                presence.backup.last_backup.is_some()
            }
            _ => presence.backup.last_backup.is_none(),
        }
}

#[cfg(test)]
mod tests {
    use crate::installation::{
        BackupReport, BackupStatus, CheckStatus, Installation, IntegrationPresence, valid_presence,
    };
    use std::time::{Duration, Instant};

    #[tokio::test]
    async fn completed_probe_remains_visible_to_a_report_while_a_cached_check_is_queued() {
        use crate::{installation::Check, sync::EasyPostClient};
        use std::future::Future;
        use std::task::{Context, Waker};

        let installation = Installation::new(true);
        let provider =
            EasyPostClient::for_loopback("unused-cached-check-key", "http://127.0.0.1:9/").unwrap();
        let mut cache = installation.provider.lock().await;
        *cache = Some((Instant::now(), Check::pass("Completed provider check")));
        let mut queued_check = Box::pin(installation.check_provider(Some(&provider)));
        // Reserve the FIFO mutex permit for another cached check. The report
        // must still return the completed result after that reader resumes.
        assert!(
            queued_check
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        );
        drop(cache);
        let (report, ()) = tokio::join!(biased; installation.report(true), queued_check);
        assert_eq!(
            report.easypost.status,
            CheckStatus::Pass,
            "{}",
            report.easypost.message
        );
    }

    #[tokio::test]
    async fn old_successful_provider_diagnostic_keeps_its_result_and_timestamp() {
        use crate::installation::Check;

        #[derive(serde::Deserialize)]
        struct DiagnosticTime {
            checked_at: Option<u64>,
        }

        let installation = Installation::new(false);
        let result: Check = serde_json::from_str(
            r#"{"status":"pass","message":"Last EasyPost check succeeded.","checked_at":1700000000}"#,
        )
        .unwrap();
        *installation.provider.lock().await =
            Some((Instant::now() - Duration::from_secs(86_400), result));

        let report = installation.report(true).await;
        assert_eq!(
            report.easypost.status,
            CheckStatus::Pass,
            "a completed diagnostic must not expire: {}",
            report.easypost.message
        );
        let observed: DiagnosticTime =
            serde_json::from_slice(&serde_json::to_vec(&report.easypost).unwrap()).unwrap();
        assert_eq!(observed.checked_at, Some(1_700_000_000));

        let disabled = installation.report(false).await;
        assert_eq!(disabled.easypost.status, CheckStatus::Attention);
        let observed: DiagnosticTime =
            serde_json::from_slice(&serde_json::to_vec(&disabled.easypost).unwrap()).unwrap();
        assert_eq!(observed.checked_at, None);
    }

    #[tokio::test]
    async fn explicit_provider_probe_replaces_a_previous_pass_and_debounces_rechecks() {
        use crate::sync::EasyPostClient;
        use std::time::{SystemTime, UNIX_EPOCH};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let installation = Installation::new(false);
        for (status, expected) in [(200, CheckStatus::Pass), (401, CheckStatus::Attention)] {
            if let Some((at, result)) = installation.provider.lock().await.as_mut() {
                *at = Instant::now() - Duration::from_secs(61);
                result.checked_at = Some(1);
            }
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let origin = format!("http://{}/", listener.local_addr().unwrap());
            let server = tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                while !request.ends_with(b"\r\n\r\n") {
                    let mut byte = [0];
                    stream.read_exact(&mut byte).await.unwrap();
                    request.push(byte[0]);
                    assert!(request.len() < 4096);
                }
                assert!(request.starts_with(b"GET /trackers?page_size=1 "));
                let body = if status == 200 {
                    r#"{"trackers":[],"has_more":false}"#
                } else {
                    "private provider message"
                };
                let response = format!(
                    "HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(response.as_bytes()).await.unwrap();
            });
            let provider =
                EasyPostClient::for_loopback("unused-diagnostic-test-key", &origin).unwrap();
            let before = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs();
            tokio::time::timeout(Duration::from_secs(3), async {
                installation.check_provider(Some(&provider)).await;
                server.await.unwrap();
            })
            .await
            .unwrap();
            let checked = installation.report(true).await.easypost;
            assert_eq!(checked.status, expected);
            let after = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs();
            assert!((before..=after).contains(&checked.checked_at.unwrap()));
            if status == 401 {
                assert_eq!(
                    checked.message,
                    "Last EasyPost check failed: EasyPost returned HTTP 401"
                );
            }
            // The HTTP fixture has closed. Rechecks within a minute retain the
            // completed result instead of attempting another connection.
            installation.check_provider(Some(&provider)).await;
            let cached = installation.report(true).await.easypost;
            assert_eq!(cached.status, checked.status);
            assert_eq!(cached.message, checked.message);
            assert_eq!(cached.checked_at, checked.checked_at);
        }
    }

    #[tokio::test]
    async fn stale_integration_cannot_claim_loaded_authenticated_or_backup_coverage() {
        let installation = Installation::new(true);
        installation
            .presence(IntegrationPresence {
                version: "0.2.0".into(),
                administrator_ids: Vec::new(),
                backup: BackupReport {
                    status: BackupStatus::Complete,
                    last_backup: Some(1_700_000_000),
                },
            })
            .await;
        let checked = installation.report(false).await;
        assert_eq!(checked.integration.status, CheckStatus::Pass);
        installation.presence.lock().await.as_mut().unwrap().0 =
            Instant::now() - Duration::from_secs(181);
        let stale = installation.report(false).await;
        assert_eq!(stale.integration.status, CheckStatus::Attention);
        assert_eq!(stale.authentication.status, CheckStatus::Attention);
        assert_eq!(stale.version.status, CheckStatus::Unavailable);
        assert_eq!(stale.backup.status, CheckStatus::Unavailable);
        assert_eq!(stale.last_backup, None);
    }

    #[test]
    fn presence_rejects_unsupported_versions_and_inconsistent_backup_reports() {
        let mut report = IntegrationPresence {
            version: "0.2.0".into(),
            administrator_ids: Vec::new(),
            backup: BackupReport {
                status: BackupStatus::Missing,
                last_backup: None,
            },
        };
        assert!(valid_presence(&report));
        report.version = "private arbitrary text".into();
        assert!(!valid_presence(&report));
        report.version = "0.2.0".into();
        report.backup.status = BackupStatus::Complete;
        assert!(!valid_presence(&report));
        report.backup.last_backup = Some(i64::MAX);
        assert!(!valid_presence(&report));
    }

    fn integration_presence(version: &str, administrators: &[&str]) -> IntegrationPresence {
        IntegrationPresence {
            version: version.into(),
            administrator_ids: administrators.iter().map(|id| (*id).into()).collect(),
            backup: BackupReport {
                status: BackupStatus::Missing,
                last_backup: None,
            },
        }
    }

    #[tokio::test]
    async fn recent_version_match_is_separate_from_connectivity_and_authentication() {
        let installation = Installation::new(true);
        for (version, expected) in [
            (env!("CARGO_PKG_VERSION").to_owned(), CheckStatus::Pass),
            ("0.2.0".to_owned(), CheckStatus::Attention),
            (
                format!("{}+different-build", env!("CARGO_PKG_VERSION")),
                CheckStatus::Attention,
            ),
        ] {
            installation
                .presence(integration_presence(&version, &[]))
                .await;
            let report = installation.report(false).await;
            assert_eq!(report.integration.status, CheckStatus::Pass);
            assert_eq!(report.authentication.status, CheckStatus::Pass);
            assert_eq!(report.version.status, expected);
            assert!(report.version.message.contains(env!("CARGO_PKG_VERSION")));
            assert!(report.version.message.contains(&version));
            if expected == CheckStatus::Attention {
                assert!(report.version.message.contains("Update both"));
            }
        }
    }

    #[tokio::test]
    async fn missing_presence_never_claims_matching_versions() {
        assert_eq!(
            Installation::new(true).report(false).await.version.status,
            CheckStatus::Unavailable
        );
        assert_eq!(
            Installation::new(false).report(false).await.version.status,
            CheckStatus::NotApplicable
        );
    }

    #[tokio::test]
    async fn administrator_access_requires_current_presence_and_exact_membership() {
        let installation = Installation::new(true);
        assert!(!installation.is_admin("admin-one").await);
        installation
            .presence(integration_presence(
                env!("CARGO_PKG_VERSION"),
                &["admin-one", "admin-two"],
            ))
            .await;
        assert!(installation.is_admin("admin-one").await);
        assert!(installation.is_admin("admin-two").await);
        assert!(!installation.is_admin("admin").await);
        assert!(!installation.is_admin("admin-one-suffix").await);
        assert!(!installation.is_admin("user").await);

        installation.presence.lock().await.as_mut().unwrap().0 =
            Instant::now() - Duration::from_secs(180);
        assert!(!installation.is_admin("admin-one").await);

        installation
            .presence(integration_presence(env!("CARGO_PKG_VERSION"), &[]))
            .await;
        assert!(!installation.is_admin("admin-one").await);
    }

    #[test]
    fn administrator_presence_rejects_duplicates_malformed_ids_and_oversized_sets() {
        let mut report = integration_presence(env!("CARGO_PKG_VERSION"), &[]);
        assert!(valid_presence(&report));
        report.administrator_ids = vec!["a".repeat(200)];
        assert!(valid_presence(&report));
        for invalid in ["", "admin one", "admin\n", "\u{00e9}", &"a".repeat(201)] {
            report.administrator_ids = vec![invalid.into()];
            assert!(
                !valid_presence(&report),
                "accepted malformed administrator ID"
            );
        }
        report.administrator_ids = vec!["same-id".into(), "same-id".into()];
        assert!(!valid_presence(&report));
        report.administrator_ids = (0..200).map(|index| format!("admin-{index}")).collect();
        assert!(valid_presence(&report));
        report.administrator_ids.push("one-too-many".into());
        assert!(!valid_presence(&report));
    }

    #[test]
    fn omitted_administrator_set_deserializes_without_granting_access() {
        let report: IntegrationPresence = serde_json::from_str(
            r#"{"version":"0.2.0","backup":{"status":"missing","last_backup":null}}"#,
        )
        .unwrap();
        assert!(report.administrator_ids.is_empty());
        assert!(valid_presence(&report));
    }
}
