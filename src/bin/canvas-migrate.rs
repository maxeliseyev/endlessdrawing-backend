//! Maintenance-only entrypoint. No HTTP listener, app credentials or admin capability.
use anyhow::{Context, Result};
use endless_drawing_server::db;
use sqlx::{
    ConnectOptions,
    postgres::{PgConnectOptions, PgPoolOptions},
};

#[tokio::main]
async fn main() -> Result<()> {
    let raw = zeroize::Zeroizing::new(
        std::env::var("DATABASE_URL").context("maintenance DATABASE_URL is required")?,
    );
    anyhow::ensure!(
        raw.starts_with("postgres://") || raw.starts_with("postgresql://"),
        "invalid maintenance database configuration"
    );
    let options: PgConnectOptions = raw
        .parse()
        .map_err(|_| anyhow::anyhow!("invalid maintenance database configuration"))?;
    let options = options.options([("lock_timeout", "30s"), ("statement_timeout", "120s")]);
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(std::time::Duration::from_secs(10))
        .connect_with(options.disable_statement_logging())
        .await
        .context("maintenance connection failed")?;
    db::MIGRATOR
        .run(&pool)
        .await
        .context("maintenance migration failed")?;
    db::verify_schema(&pool).await?;
    if let Ok(role) = std::env::var("RUNTIME_DATABASE_ROLE")
        && !role.is_empty()
    {
        db::grant_runtime_access(&mut *pool.acquire().await?, &role).await?;
    }
    pool.close().await;
    println!("Maintenance migrations and manifest verified");
    Ok(())
}
