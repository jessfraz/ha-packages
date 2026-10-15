use anyhow::{Context, Result, ensure};
use clap::Parser;
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::{
    net::{IpAddr, SocketAddr},
    path::PathBuf,
};

#[derive(Parser)]
#[command(version, about)]
pub struct Config {
    #[arg(long, env = "SHIPMENTS_BIND", default_value = "127.0.0.1:8080")]
    pub bind: SocketAddr,
    #[arg(long, env = "SHIPMENTS_DATABASE", default_value = "shipments.sqlite3")]
    pub database: PathBuf,
    #[arg(long, env = "SHIPMENTS_API_TOKEN", hide_env_values = true)]
    pub api_token: Option<String>,
    #[arg(long, env = "SHIPMENTS_INTEGRATION_TOKEN", hide_env_values = true)]
    pub integration_token: Option<String>,
    #[arg(long, env = "EASYPOST_API_KEY", hide_env_values = true)]
    pub easypost_api_key: Option<String>,
    #[arg(long, env = "AFTERSHIP_API_KEY", hide_env_values = true)]
    pub aftership_api_key: Option<String>,
    #[arg(long, env = "SHIPMENTS_INGRESS_BIND")]
    pub ingress_bind: Option<SocketAddr>,
    #[arg(long, default_value = "172.30.32.2")]
    pub ingress_peer: IpAddr,
    #[arg(long)]
    pub options: Option<PathBuf>,
    /// Write the generated OpenAPI document and exit without starting a server.
    #[arg(long)]
    pub openapi: Option<PathBuf>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Options {
    easypost_api_key: Option<String>,
    aftership_api_key: Option<String>,
}

impl Config {
    pub fn load() -> Result<Self> {
        let mut config = Self::parse();
        if let Some(path) = &config.options {
            let options: Options =
                serde_json::from_slice(&std::fs::read(path).context("read app options")?)
                    .context("invalid app options")?;
            let credentials =
                Credentials::load_or_create(&path.with_file_name("credentials.json"))?;
            config.api_token = Some(credentials.owner);
            config.integration_token = Some(credentials.integration);
            config.easypost_api_key = config
                .easypost_api_key
                .or(options.easypost_api_key.filter(|value| !value.is_empty()));
            config.aftership_api_key = config
                .aftership_api_key
                .or(options.aftership_api_key.filter(|value| !value.is_empty()));
        }
        if config.openapi.is_none() {
            let token = config
                .api_token
                .as_deref()
                .context("set SHIPMENTS_API_TOKEN (at least 32 characters)")?;
            ensure!(
                token.len() >= 32,
                "API token must be at least 32 characters"
            );
            ensure!(
                config
                    .integration_token
                    .as_ref()
                    .is_none_or(|token| (32..=256).contains(&token.len())
                        && token.bytes().all(|byte| byte.is_ascii_graphic())),
                "integration credential must contain 32..256 printable ASCII characters"
            );
            ensure!(
                config.integration_token.as_ref() != config.api_token.as_ref(),
                "integration and owner credentials must be distinct"
            );
        }
        Ok(config)
    }
}

#[derive(Deserialize, Serialize)]
struct Credentials {
    owner: String,
    integration: String,
}

impl Credentials {
    fn load_or_create(path: &std::path::Path) -> Result<Self> {
        use std::fs::OpenOptions;
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

        if !path.try_exists().context("check private app credentials")? {
            let credentials = Self {
                owner: random_token()?,
                integration: random_token()?,
            };
            let temporary = path.with_extension(format!("{}.tmp", uuid::Uuid::now_v7()));
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&temporary)
                .context("create private app credentials")?;
            let result = (|| -> Result<()> {
                file.write_all(&serde_json::to_vec(&credentials)?)
                    .context("write private app credentials")?;
                file.sync_all().context("persist private app credentials")?;
                // Publish a complete file atomically, without overwriting a
                // credential created by another process starting concurrently.
                match std::fs::hard_link(&temporary, path) {
                    Ok(()) => std::fs::File::open(path.parent().context("credential directory")?)?
                        .sync_all()
                        .context("persist credential directory")?,
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                    Err(error) => return Err(error).context("publish private app credentials"),
                }
                Ok(())
            })();
            std::fs::remove_file(&temporary).context("remove temporary credentials")?;
            result?;
        }
        let metadata =
            std::fs::symlink_metadata(path).context("read app credential permissions")?;
        ensure!(
            metadata.is_file() && metadata.permissions().mode() & 0o077 == 0,
            "app credentials must be an owner-only regular file"
        );
        let file = std::fs::File::open(path).context("open private app credentials")?;
        let mut bytes = Vec::new();
        file.take(4097).read_to_end(&mut bytes)?;
        ensure!(bytes.len() <= 4096, "invalid private app credentials");
        let value: Self =
            serde_json::from_slice(&bytes).context("invalid private app credentials")?;
        ensure!(
            value.owner.len() >= 32
                && value.integration.len() >= 32
                && value.owner != value.integration,
            "invalid private app credentials"
        );
        Ok(value)
    }
}

pub(crate) fn random_token() -> Result<String> {
    let mut bytes = [0_u8; 32];
    getrandom::fill(&mut bytes).map_err(|_| anyhow::anyhow!("generate private credential"))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

#[cfg(test)]
mod tests {
    use crate::config::Credentials;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn app_credentials_are_private_distinct_and_stable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("credentials.json");
        let first = Credentials::load_or_create(&path).unwrap();
        let restored = Credentials::load_or_create(&path).unwrap();
        assert_eq!(first.owner.len(), 64);
        assert_ne!(first.owner, first.integration);
        assert_eq!(first.owner, restored.owner);
        assert_eq!(first.integration, restored.integration);
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(Credentials::load_or_create(&path).is_err());
    }

    #[test]
    fn simultaneous_starts_preserve_one_complete_credential_file() {
        let directory = tempfile::tempdir().unwrap();
        let path = std::sync::Arc::new(directory.path().join("credentials.json"));
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let starts: Vec<_> = (0..2)
            .map(|_| {
                let path = path.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    Credentials::load_or_create(&path).unwrap()
                })
            })
            .collect();
        let results: Vec<_> = starts
            .into_iter()
            .map(|start| start.join().unwrap())
            .collect();
        assert_eq!(results[0].owner, results[1].owner);
        assert_eq!(results[0].integration, results[1].integration);
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
    }
}
