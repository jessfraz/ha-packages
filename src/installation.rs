//! Live install checks. A recent authenticated adapter heartbeat establishes
//! that Home Assistant actually loaded the integration and reached this app.

use crate::sync::EasyPostClient;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex;

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
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct InstallationReport {
    pub backend: Check,
    pub integration: Check,
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

    pub async fn check_provider(&self, provider: Option<&EasyPostClient>) {
        let Some(provider) = provider else {
            return;
        };
        // Serialize outbound probes separately from their short-lived cached
        // results. Readers never wait for a provider network request.
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
        let result = match provider.check_connection().await {
            Ok(()) => Check::pass("EasyPost accepted the key. Read-only tracking access works."),
            Err(error) => Check {
                status: CheckStatus::Attention,
                message: error.to_string(),
            },
        };
        *self.provider.lock().await = Some((Instant::now(), result));
    }

    pub async fn report(&self, provider_enabled: bool) -> InstallationReport {
        let presence = self.presence.lock().await;
        let recent = presence
            .as_ref()
            .filter(|(at, _)| at.elapsed() < Duration::from_secs(180));
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
        let easypost = if !provider_enabled {
            Check {
                status: CheckStatus::NotApplicable,
                message: "EasyPost is not configured. Manual shipment records still work.".into(),
            }
        } else {
            let provider = self.provider.lock().await;
            if let Some((at, result)) = provider.as_ref() {
                if at.elapsed() < Duration::from_secs(300) {
                    result.clone()
                } else {
                    Check::attention("The EasyPost check has expired. Run checks again.")
                }
            } else {
                Check::attention(
                    "EasyPost is configured. Run checks to verify the key and connectivity.",
                )
            }
        };
        let backup = match recent.map(|(_, report)| &report.backup.status) {
            None if !self.supervised => Check { status: CheckStatus::NotApplicable, message: "Standalone server. Stop it before backing up its database and credentials. Home Assistant backup inventory is not available here.".into() },
            Some(BackupStatus::Complete) => Check::pass("A Home Assistant backup includes this app and the integration configuration. Restore has not been tested."),
            Some(BackupStatus::AppOnly) => Check::attention("A backup includes the app, but not Home Assistant configuration. Back up both to preserve pairing."),
            Some(BackupStatus::Outdated) => Check::attention("The available app backup predates pairing. Create a new backup containing Shipments and Home Assistant configuration to preserve this connection."),
            Some(BackupStatus::Missing) => Check::attention("No backup includes this app. Create a Home Assistant backup containing Shipments and Home Assistant configuration."),
            Some(BackupStatus::Standalone) => Check { status: CheckStatus::NotApplicable, message: "Standalone backend. Stop it before backing up its data and credentials; back up Home Assistant configuration separately.".into() },
            _ => Check { status: CheckStatus::Unavailable, message: "Backup status is unavailable. Check Settings → System → Backups and include Shipments and Home Assistant configuration.".into() },
        };
        InstallationReport {
            backend: Check::pass("The app is running and SQLite is responding."),
            integration,
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
        }
    }
    fn attention(message: &str) -> Self {
        Self {
            status: CheckStatus::Attention,
            message: message.into(),
        }
    }
}

pub fn valid_presence(presence: &IntegrationPresence) -> bool {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_secs())
        .unwrap_or_default();
    semver::Version::parse(&presence.version).is_ok()
        && presence.version.len() <= 64
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
    async fn stale_integration_cannot_claim_loaded_authenticated_or_backup_coverage() {
        let installation = Installation::new(true);
        installation
            .presence(IntegrationPresence {
                version: "0.2.0".into(),
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
        assert_eq!(stale.backup.status, CheckStatus::Unavailable);
        assert_eq!(stale.last_backup, None);
    }

    #[test]
    fn presence_rejects_unsupported_versions_and_inconsistent_backup_reports() {
        let mut report = IntegrationPresence {
            version: "0.2.0".into(),
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
}
