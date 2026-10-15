use anyhow::{Context as _, Result};
use shipments::{
    api,
    config::Config,
    shipping_api::{ShippingApi, easypost::EasyPostClient},
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
    let provider: Option<Arc<dyn ShippingApi>> = config
        .easypost_api_key
        .map(|key| {
            EasyPostClient::new(key)
                .map(|client| Arc::new(client.with_logger(log.clone())) as Arc<dyn ShippingApi>)
        })
        .transpose()?;
    let mut context = api::Context::new(store.clone(), token, None, provider.clone());
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
    let (stop_tx, mut stop_rx) = tokio::sync::watch::channel(false);
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
    let worker = if let Some(provider) = provider {
        let worker = SyncWorker::new(store.pool().clone(), provider);
        let log = log.clone();
        Some(tokio::spawn(async move {
            loop {
                tokio::select! {
                    _=stop_rx.changed()=>break,
                    result=worker.tick()=>{
                        let delay=match result {Ok(true)=>std::time::Duration::from_millis(100),Ok(false)=>std::time::Duration::from_secs(5),Err(_)=>{slog::error!(log, "tracking_sync"; "operation" => "sync", "outcome" => "error", "error_code" => "database_error");std::time::Duration::from_secs(30)}};
                        tokio::select! {_=stop_rx.changed()=>break,_=tokio::time::sleep(delay)=>{}}
                    }
                }
            }
        }))
    } else {
        None
    };
    shutdown().await?;
    // Stop accepting mutations and drain active handlers before closing SQLite.
    server
        .close()
        .await
        .map_err(|error| anyhow::anyhow!("close API: {error}"))?;
    if let Some(ingress) = ingress {
        ingress
            .close()
            .await
            .map_err(|error| anyhow::anyhow!("close ingress: {error}"))?;
    }
    let _ = stop_tx.send(true);
    if let Some(worker) = worker {
        worker.await?;
    }
    if let Some(discovery) = discovery {
        discovery.await?;
    }
    store.close().await?;
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
