//! Closed, operator-only provisioning. Not exposed by any HTTP route.

use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;
use zeroize::Zeroizing;

use super::{
    AdminError, auth,
    crypto::{self, Token},
};

/// A short-lived, account-bound enrollment capability. Intentionally not Debug/Serialize.
pub struct EnrollmentGrant {
    account_id: Uuid,
    token: Token,
    expires_at: DateTime<Utc>,
}
impl EnrollmentGrant {
    /// Account created/reset by the closed operation.
    pub fn account_id(&self) -> Uuid {
        self.account_id
    }
    /// Expiration on the database clock.
    pub fn expires_at(&self) -> DateTime<Utc> {
        self.expires_at
    }
    /// Exposes the enrollment secret for a trusted operator terminal only. Never log it.
    pub fn secret(&self) -> String {
        self.token.encoded()
    }
}

/// Creates an account and two-use, 15-minute enrollment grant; never an admin session.
///
/// # Errors
/// Rejects invalid login/password, duplicate login and database/crypto failures.
pub async fn create(
    pool: &PgPool,
    login: &str,
    password: Zeroizing<String>,
) -> Result<EnrollmentGrant, AdminError> {
    let login = crypto::login(login)?;
    let hash = tokio::task::spawn_blocking(move || crypto::hash_password(&password))
        .await
        .map_err(|_| AdminError::Unavailable)??;
    let mut transaction = pool.begin().await?;
    let id = Uuid::new_v4();
    let inserted = sqlx::query("INSERT INTO admin_accounts(id, login, password_hash) VALUES ($1, $2, $3) ON CONFLICT (login) DO NOTHING")
        .bind(id).bind(login).bind(hash).execute(&mut *transaction).await?;
    if inserted.rows_affected() != 1 {
        return Err(AdminError::Conflict);
    }
    let grant = grant(&mut transaction, id, 1).await?;
    auth::audit(&mut transaction, Some(id), "account_created").await?;
    transaction.commit().await?;
    Ok(grant)
}

/// Closed recovery: changes password and revokes ALL old factors, sessions and ceremonies.
/// Re-enables the account but requires a new UV passkey before login; TOTP is revoked too.
///
/// # Errors
/// Rejects unknown account, invalid input and database/crypto failures.
pub async fn recover(
    pool: &PgPool,
    login: &str,
    password: Zeroizing<String>,
) -> Result<EnrollmentGrant, AdminError> {
    let login = crypto::login(login)?;
    let hash = tokio::task::spawn_blocking(move || crypto::hash_password(&password))
        .await
        .map_err(|_| AdminError::Unavailable)??;
    let mut transaction = pool.begin().await?;
    let (id,): (Uuid,) =
        sqlx::query_as("SELECT id FROM admin_accounts WHERE login = $1 FOR UPDATE")
            .bind(login)
            .fetch_optional(&mut *transaction)
            .await?
            .ok_or(AdminError::Unauthorized)?;
    let version: i64 = sqlx::query_scalar("UPDATE admin_accounts SET password_hash = $1, enabled = true, auth_version = auth_version + 1 WHERE id = $2 RETURNING auth_version")
        .bind(hash).bind(id).fetch_one(&mut *transaction).await?;
    sqlx::query("UPDATE admin_credentials SET revoked_at = clock_timestamp() WHERE account_id = $1 AND revoked_at IS NULL")
        .bind(id).execute(&mut *transaction).await?;
    revoke(&mut transaction, id).await?;
    let grant = grant(&mut transaction, id, version).await?;
    auth::audit(&mut transaction, Some(id), "account_recovered").await?;
    transaction.commit().await?;
    Ok(grant)
}

/// Disables an administrator and revokes all sessions/enrollment capabilities.
///
/// # Errors
/// Rejects unknown login or database failures.
pub async fn disable(pool: &PgPool, login: &str) -> Result<(), AdminError> {
    let login = crypto::login(login)?;
    let mut transaction = pool.begin().await?;
    let id: Uuid = sqlx::query_scalar("SELECT id FROM admin_accounts WHERE login = $1 FOR UPDATE")
        .bind(login)
        .fetch_optional(&mut *transaction)
        .await?
        .ok_or(AdminError::Unauthorized)?;
    sqlx::query(
        "UPDATE admin_accounts SET enabled = false, auth_version = auth_version + 1 WHERE id = $1",
    )
    .bind(id)
    .execute(&mut *transaction)
    .await?;
    revoke(&mut transaction, id).await?;
    auth::audit(&mut transaction, Some(id), "account_disabled").await?;
    transaction.commit().await?;
    Ok(())
}

async fn revoke(connection: &mut sqlx::PgConnection, account: Uuid) -> Result<(), AdminError> {
    sqlx::query("UPDATE admin_sessions SET revoked_at = clock_timestamp() WHERE account_id = $1 AND revoked_at IS NULL")
        .bind(account).execute(&mut *connection).await?;
    sqlx::query("DELETE FROM admin_preauth WHERE account_id = $1")
        .bind(account)
        .execute(&mut *connection)
        .await?;
    sqlx::query("DELETE FROM admin_totp WHERE account_id = $1")
        .bind(account)
        .execute(&mut *connection)
        .await?;
    sqlx::query("DELETE FROM admin_enrollments WHERE account_id = $1")
        .bind(account)
        .execute(connection)
        .await?;
    Ok(())
}
async fn grant(
    connection: &mut sqlx::PgConnection,
    account_id: Uuid,
    version: i64,
) -> Result<EnrollmentGrant, AdminError> {
    let token = Token::random()?;
    let expires_at = sqlx::query_scalar(
        "INSERT INTO admin_enrollments(token_digest, account_id, auth_version, expires_at)
         VALUES ($1, $2, $3, clock_timestamp() + interval '15 minutes') RETURNING expires_at",
    )
    .bind(token.digest().as_slice())
    .bind(account_id)
    .bind(version)
    .fetch_one(connection)
    .await?;
    Ok(EnrollmentGrant {
        account_id,
        token,
        expires_at,
    })
}
