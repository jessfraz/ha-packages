use anyhow::{Context, Result, ensure};
use clap::Parser;
use serde::Deserialize;
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
    #[arg(long, env = "SHIPMENTS_AGENT_TOKEN", hide_env_values = true)]
    pub agent_token: Option<String>,
    #[arg(long, env = "EASYPOST_API_KEY", hide_env_values = true)]
    pub easypost_api_key: Option<String>,
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
    api_token: String,
    agent_token: Option<String>,
    easypost_api_key: Option<String>,
}

impl Config {
    pub fn load() -> Result<Self> {
        let mut config = Self::parse();
        if let Some(path) = &config.options {
            let options: Options =
                serde_json::from_slice(&std::fs::read(path).context("read app options")?)
                    .context("invalid app options")?;
            config.api_token.get_or_insert(options.api_token);
            config.agent_token = config
                .agent_token
                .or(options.agent_token.filter(|value| !value.is_empty()));
            config.easypost_api_key = config
                .easypost_api_key
                .or(options.easypost_api_key.filter(|value| !value.is_empty()));
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
            if let Some(agent) = &config.agent_token {
                ensure!(
                    agent.len() >= 32 && agent != token,
                    "agent token must be distinct and at least 32 characters"
                );
            }
        }
        Ok(config)
    }
}
