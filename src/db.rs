//! PostgreSQL initialization and embedded migrations.

use sqlx::{ConnectOptions, PgPool, migrate::Migrator, postgres::PgPoolOptions};

use crate::config::Config;

/// Embedded, versioned schema. Startup must finish migrations before serving HTTP.
pub static MIGRATOR: Migrator = sqlx::migrate!("./migrations");

/// Opens the pool with bounded waits and a per-statement database timeout.
///
/// # Errors
/// Returns the PostgreSQL/SQLx error if connection establishment or setup fails.
pub async fn connect(config: &Config) -> Result<PgPool, sqlx::Error> {
    let statement_timeout = format!("{}ms", config.database_query_timeout.as_millis());
    PgPoolOptions::new()
        .max_connections(config.database_max_connections)
        .acquire_timeout(config.database_acquire_timeout)
        .after_connect(move |connection, _| {
            let statement_timeout = statement_timeout.clone();
            Box::pin(async move {
                sqlx::query("SELECT set_config('statement_timeout', $1, false)")
                    .bind(statement_timeout)
                    .execute(connection)
                    .await?;
                Ok(())
            })
        })
        .connect_with(config.database.clone().disable_statement_logging())
        .await
}
