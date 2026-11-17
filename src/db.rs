//! PostgreSQL initialization and embedded migrations.

use sqlx::{
    ConnectOptions, Connection, PgConnection, PgPool, migrate::Migrator, postgres::PgPoolOptions,
};

use crate::config::Config;

/// Embedded, versioned schema. Startup must finish migrations before serving HTTP.
pub static MIGRATOR: Migrator = sqlx::migrate!("./migrations");

/// Startup policy errors contain no connection settings or credentials.
#[derive(Debug, thiserror::Error)]
pub enum DatabasePolicyError {
    /// Applied versions/checksums must exactly match this binary.
    #[error("database schema does not match this binary; run the maintenance migrator")]
    SchemaMismatch,
    /// Public runtime must not own persistent schema objects or privileged memberships.
    #[error("unsafe runtime database role; refuse to start with DDL or cluster privileges")]
    UnsafeRuntimeRole,
    /// Database failure; connection settings remain non-printable.
    #[error("could not verify database startup policy")]
    Database(#[from] sqlx::Error),
}

/// Read-only manifest validation: never CREATE/ALTER or take migration locks as runtime.
///
/// # Errors
/// Rejects missing/dirty/mismatched/newer migrations or database query failure.
pub async fn verify_schema(
    executor: impl sqlx::Executor<'_, Database = sqlx::Postgres>,
) -> Result<(), DatabasePolicyError> {
    let actual: Vec<(i64, Vec<u8>, bool)> =
        sqlx::query_as("SELECT version, checksum, success FROM _sqlx_migrations ORDER BY version")
            .fetch_all(executor)
            .await?;
    let expected: Vec<_> = MIGRATOR
        .iter()
        .filter(|migration| migration.migration_type.is_up_migration())
        .collect();
    if actual.len() != expected.len()
        || actual
            .iter()
            .zip(expected)
            .any(|((version, checksum, success), migration)| {
                !success
                    || *version != migration.version
                    || checksum.as_slice() != migration.checksum.as_ref()
            })
    {
        return Err(DatabasePolicyError::SchemaMismatch);
    }
    Ok(())
}

/// Checks the effective current_user, not the migration/maintenance connection.
///
/// # Errors
/// Rejects superuser/role/db/replication/RLS bypass privileges, persistent ownership,
/// schema/database CREATE and ANY role membership (including NOINHERIT/SET ROLE).
pub async fn verify_runtime_role(
    executor: impl sqlx::Executor<'_, Database = sqlx::Postgres>,
) -> Result<(), DatabasePolicyError> {
    let unsafe_role: bool = sqlx::query_scalar(
        "SELECT r.rolsuper OR r.rolcreatedb OR r.rolcreaterole OR r.rolreplication OR r.rolbypassrls
         OR EXISTS(SELECT 1 FROM pg_namespace n WHERE n.nspname !~ '^pg_' AND n.nspname<>'information_schema' AND (n.nspowner=r.oid OR has_schema_privilege(current_user,n.oid,'CREATE')))
         OR has_database_privilege(current_user, current_database(), 'CREATE')
         OR EXISTS(SELECT 1 FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace WHERE n.nspname !~ '^pg_' AND n.nspname<>'information_schema' AND c.relowner=r.oid)
         OR EXISTS(SELECT 1 FROM pg_proc p JOIN pg_namespace n ON n.oid=p.pronamespace WHERE n.nspname !~ '^pg_' AND n.nspname<>'information_schema' AND p.proowner=r.oid)
         OR EXISTS(SELECT 1 FROM pg_roles other WHERE other.oid<>r.oid AND pg_has_role(current_user,other.oid,'MEMBER'))
         FROM pg_roles r WHERE r.rolname=current_user",
    ).fetch_one(executor).await?;
    if unsafe_role {
        return Err(DatabasePolicyError::UnsafeRuntimeRole);
    }
    Ok(())
}

/// Grants only application DML/sequence use; migration metadata stays SELECT-only.
/// Called exclusively by the maintenance entrypoint, never HTTP/runtime startup.
///
/// # Errors
/// Rejects invalid identifiers and database grant failures.
pub async fn grant_runtime_access(
    connection: &mut PgConnection,
    role: &str,
) -> Result<(), DatabasePolicyError> {
    if role.is_empty()
        || role.len() > 63
        || !role
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_')
    {
        return Err(DatabasePolicyError::UnsafeRuntimeRole);
    }
    let mut tx = connection.begin().await?;
    let tables: Vec<String>=sqlx::query_scalar("SELECT c.relname FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace WHERE n.nspname='public' AND c.relkind IN ('r','p') AND c.relname<>'_sqlx_migrations'").fetch_all(&mut *tx).await?;
    for table in tables {
        let table = table.replace('"', "\"\"");
        sqlx::query(&format!(
            "GRANT SELECT,INSERT,UPDATE,DELETE ON TABLE public.\"{table}\" TO \"{role}\""
        ))
        .execute(&mut *tx)
        .await?;
    }
    sqlx::query(&format!("GRANT USAGE ON SCHEMA public TO \"{role}\""))
        .execute(&mut *tx)
        .await?;
    sqlx::query(&format!(
        "GRANT USAGE,SELECT ON ALL SEQUENCES IN SCHEMA public TO \"{role}\""
    ))
    .execute(&mut *tx)
    .await?;
    sqlx::query(&format!(
        "REVOKE ALL ON public._sqlx_migrations FROM \"{role}\""
    ))
    .execute(&mut *tx)
    .await?;
    sqlx::query(&format!(
        "GRANT SELECT ON public._sqlx_migrations TO \"{role}\""
    ))
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

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
