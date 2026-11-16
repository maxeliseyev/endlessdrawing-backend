use anyhow::{Context, Result};
use endless_drawing_server::{
    admin::{self, AdminService},
    app,
    config::Config,
    db,
    realtime::Realtime,
};
use std::time::Duration;
use tracing_subscriber::EnvFilter;

fn main() -> Result<()> {
    // Load local configuration before starting the multithreaded runtime.
    match dotenvy::dotenv() {
        Ok(_) => {}
        Err(dotenvy::Error::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => anyhow::bail!("could not load .env; check its syntax and permissions"),
    }
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .try_init()
        .map_err(|_| anyhow::anyhow!("could not initialize logging"))?;
    let config = Config::from_env()?;
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("could not start Tokio runtime")?
        .block_on(run(config))
}

async fn run(config: Config) -> Result<()> {
    if let Some(directory) = &config.static_dir {
        let index = directory.join("index.html");
        let metadata = tokio::fs::metadata(index)
            .await
            .context("STATIC_DIR must contain a readable frontend index.html")?;
        anyhow::ensure!(metadata.is_file(), "STATIC_DIR index.html must be a file");
    }
    let pool = db::connect(&config)
        .await
        .context("could not connect to PostgreSQL")?;
    db::MIGRATOR
        .run(&pool)
        .await
        .context("could not apply database migrations")?;
    let listener = tokio::net::TcpListener::bind(config.bind_addr)
        .await
        .context("could not bind HTTP listener")?;
    let (realtime, mut writer) =
        Realtime::start(pool.clone(), config.stroke_limits, config.realtime)?;
    let shutdown = realtime.shutdown_token();
    let signal_shutdown = shutdown.clone();
    let administrator = match config.admin {
        Some(settings) => Some(
            AdminService::new(pool.clone(), &realtime, settings)
                .await
                .context(
                    "could not initialize administration; check passkey RP/origin configuration",
                )?,
        ),
        None => None,
    };
    let application = app::router(
        pool.clone(),
        config.database_query_timeout,
        config.static_dir.clone(),
    )
    .merge(realtime.router())
    .merge(admin::router(administrator, config.static_dir));
    tracing::info!(address = %listener.local_addr()?, "backend ready");
    let result = axum::serve(
        listener,
        application.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(async move {
        shutdown_signal().await;
        signal_shutdown.cancel();
    })
    .await;
    shutdown.cancel();
    match tokio::time::timeout(Duration::from_secs(30), &mut writer).await {
        Ok(result) => result.context("mutation worker failed")?,
        Err(_) => {
            writer.abort();
            let _ = writer.await;
            tracing::warn!("mutation worker exceeded shutdown timeout");
        }
    }
    tokio::time::timeout(Duration::from_secs(30), pool.close())
        .await
        .context("database pool exceeded shutdown timeout")?;
    result.context("HTTP server failed")
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        let mut terminate =
            match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                Ok(signal) => signal,
                Err(error) => {
                    tracing::error!(%error, "could not register SIGTERM handler; shutting down");
                    return;
                }
            };
        tokio::select! {
            result = tokio::signal::ctrl_c() => {
                if let Err(error) = result {
                    tracing::error!(%error, "could not receive Ctrl+C signal");
                }
            }
            _ = terminate.recv() => {}
        }
    }
    #[cfg(not(unix))]
    if let Err(error) = tokio::signal::ctrl_c().await {
        tracing::error!(%error, "could not receive Ctrl+C signal");
    }
    tracing::info!("shutting down");
}
