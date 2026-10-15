use anyhow::{Context as _, Result};
use shipments::{
    api,
    config::Config,
    shipping_api::{
        aftership::AfterShipClient, doordash::DoorDashClient, easypost::EasyPostClient,
    },
    store::Store,
    supervisor::Supervisor,
    sync::SyncWorker,
};
use std::sync::Arc;

#[tokio::main]
async fn main() -> Result<()> {
    let config = Config::load()?;
    if let Some(path) = &config.openapi {
        std::fs::write(path, serde_json::to_string_pretty(&api::openapi()?)?)?;
        return Ok(());
    }
    let log = shipments::logging::logger();
    let store = Store::open(&config.database).await?;
    let token = config
        .api_token
        .as_deref()
        .context("missing owner credential")?;
    let mut context = api::Context::new(store.clone(), token, None, None)
        .with_provider(Arc::new(DoorDashClient::new()?.with_logger(log.clone())));
    if let Some(key) = config.easypost_api_key {
        context =
            context.with_provider(Arc::new(EasyPostClient::new(key)?.with_logger(log.clone())));
    }
    if let Some(key) = config.aftership_api_key {
        context = context.with_provider(Arc::new(
            AfterShipClient::new(key)?.with_logger(log.clone()),
        ));
    }
    let providers = context.providers().to_vec();
    if let Some(token) = &config.integration_token {
        context = context.with_integration(token);
    }
    let server = api::start_with_logger(config.bind, Arc::new(context.clone()), log.clone())?;
    let ingress = if let Some(bind) = config.ingress_bind {
        Some(api::start_with_logger(
            bind,
            Arc::new(context.for_ingress(config.ingress_peer)),
            log.clone(),
        )?)
    } else {
        None
    };
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    let discovery = if let (Some(supervisor), Some(integration_token)) =
        (Supervisor::from_environment()?, config.integration_token)
    {
        let mut discovery_stop = stop_rx.clone();
        let log = log.clone();
        Some(tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = discovery_stop.changed() => break,
                    result = supervisor.announce(&integration_token) => {
                        if result.is_err() { slog::warn!(log, "integration_discovery"; "operation" => "discovery", "outcome" => "error", "error_code" => "discovery_unavailable"); }
                    }
                }
                tokio::select! {
                    _ = discovery_stop.changed() => break,
                    _ = tokio::time::sleep(std::time::Duration::from_secs(60)) => {}
                }
            }
        }))
    } else {
        None
    };
    let workers: Vec<_> = providers.into_iter().map(|provider| {
        let worker = SyncWorker::new(store.pool().clone(), provider);
        let log = log.clone();
        let mut worker_stop = stop_rx.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = worker_stop.changed() => break,
                    result = worker.tick() => {
                        let delay = match result {
                            Ok(true) => std::time::Duration::from_millis(100),
                            Ok(false) => std::time::Duration::from_secs(5),
                            Err(_) => {
                                slog::error!(log, "tracking_sync"; "operation" => "sync", "outcome" => "error", "error_code" => "database_error");
                                std::time::Duration::from_secs(30)
                            }
                        };
                        tokio::select! {
                            _ = worker_stop.changed() => break,
                            _ = tokio::time::sleep(delay) => {}
                        }
                    }
                }
            }
        })
    }).collect();
    let mut shutdown_error = shutdown().await.err();
    // Stop accepting mutations and drain active handlers before stopping the
    // workers. Every provider task must finish before SQLite closes, even when
    // another task or server reports an error during shutdown.
    if let Err(error) = server.close().await {
        shutdown_error.get_or_insert_with(|| anyhow::anyhow!("close API: {error}"));
    }
    if let Some(ingress) = ingress
        && let Err(error) = ingress.close().await
    {
        shutdown_error.get_or_insert_with(|| anyhow::anyhow!("close ingress: {error}"));
    }
    let _ = stop_tx.send(true);
    for worker in workers {
        if let Err(error) = worker.await {
            shutdown_error.get_or_insert_with(|| anyhow::anyhow!("join tracking worker: {error}"));
        }
    }
    if let Some(discovery) = discovery
        && let Err(error) = discovery.await
    {
        shutdown_error.get_or_insert_with(|| anyhow::anyhow!("join discovery: {error}"));
    }
    if let Err(error) = store.close().await {
        shutdown_error.get_or_insert_with(|| error.into());
    }
    if let Some(error) = shutdown_error {
        return Err(error);
    }

    Ok(())
}

async fn shutdown() -> Result<()> {
    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        tokio::select! {result=tokio::signal::ctrl_c()=>result?,_=term.recv()=>{}}
    }
    #[cfg(not(unix))]
    tokio::signal::ctrl_c().await?;
    Ok(())
}
