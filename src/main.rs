use anyhow::{Context as _, Result};
use shipments::{
    api,
    config::Config,
    store::Store,
    sync::{EasyPostClient, SyncWorker},
};
use std::sync::Arc;

#[tokio::main]
async fn main() -> Result<()> {
    let config = Config::load()?;
    if let Some(path) = &config.openapi {
        std::fs::write(path, serde_json::to_string_pretty(&api::openapi()?)?)?;
        return Ok(());
    }
    let store = Store::open(&config.database).await?;
    let token = config
        .api_token
        .as_deref()
        .context("missing owner credential")?;
    let context = Arc::new(api::Context::new(
        store.clone(),
        token,
        config.agent_token.as_deref(),
        None,
        config.easypost_api_key.is_some(),
    ));
    let server = api::start(config.bind, context)?;
    let ingress = if let Some(bind) = config.ingress_bind {
        Some(api::start(
            bind,
            Arc::new(api::Context::new(
                store.clone(),
                token,
                None,
                Some(config.ingress_peer),
                config.easypost_api_key.is_some(),
            )),
        )?)
    } else {
        None
    };
    let (stop_tx, mut stop_rx) = tokio::sync::watch::channel(false);
    let worker = if let Some(key) = config.easypost_api_key {
        let worker = SyncWorker::new(store.pool().clone(), EasyPostClient::new(key)?);
        Some(tokio::spawn(async move {
            loop {
                tokio::select! {
                    _=stop_rx.changed()=>break,
                    result=worker.tick()=>{
                        let delay=match result {Ok(true)=>std::time::Duration::from_millis(100),Ok(false)=>std::time::Duration::from_secs(5),Err(_)=>{eprintln!("tracking worker database error; retrying");std::time::Duration::from_secs(30)}};
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
