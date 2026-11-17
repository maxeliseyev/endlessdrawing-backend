//! Durable opaque sessions, expiry/revocation and account/session lock ordering.

use axum::http::HeaderMap;
use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::{FromRow, PgConnection};
use std::{net::IpAddr, time::Duration};
use uuid::Uuid;
use webauthn_rs::prelude::Passkey;
use zeroize::Zeroizing;

use super::{
    AdminError, AdminService,
    crypto::{self, PREAUTH_COOKIE, SESSION_COOKIE, Token},
};

#[derive(FromRow)]
pub(crate) struct Account {
    pub id: Uuid,
    pub login: String,
    pub password_hash: String,
    pub auth_version: i64,
    pub enabled: bool,
}

#[derive(FromRow)]
pub(crate) struct CredentialRow {
    pub credential_id: Vec<u8>,
    pub credential: sqlx::types::Json<Passkey>,
    pub counter: i64,
}

// Not serializable/debuggable. Constructed from a validated cookie, never client JSON.
pub(crate) struct SessionProof {
    pub account_id: Uuid,
    pub token_digest: [u8; 32],
    pub origin: String,
    pub policy_digest: [u8; 32],
}

#[derive(Serialize)]
pub(crate) struct AccountView {
    pub id: Uuid,
    pub login: String,
}
#[derive(Serialize)]
pub(crate) struct SessionReply {
    authenticated: bool,
    account: AccountView,
    csrf_token: String,
    expires_at: DateTime<Utc>,
    idle_expires_at: DateTime<Utc>,
    auth_method: String,
}
#[derive(Serialize)]
pub(crate) struct OptionsReply {
    pub csrf_token: String,
    pub expires_at: DateTime<Utc>,
    pub authenticator_policy: &'static str,
}

pub(crate) struct SessionInfo {
    pub account: AccountView,
    pub expires_at: DateTime<Utc>,
    pub last_seen_at: DateTime<Utc>,
    pub auth_method: String,
    pub created_at: DateTime<Utc>,
}

impl SessionInfo {
    pub fn reply(self, token: &Token, origin: &str) -> Result<SessionReply, AdminError> {
        Ok(SessionReply {
            authenticated: true,
            auth_method: self.auth_method,
            account: self.account,
            csrf_token: token.csrf("session", origin)?,
            expires_at: self.expires_at,
            idle_expires_at: (self.last_seen_at + chrono::Duration::minutes(15))
                .min(self.expires_at),
        })
    }
}

pub(crate) async fn audit(
    connection: &mut PgConnection,
    account: Option<Uuid>,
    event: &str,
) -> Result<(), AdminError> {
    sqlx::query("INSERT INTO admin_security_audit(account_id, event) VALUES ($1, $2)")
        .bind(account)
        .bind(event)
        .execute(connection)
        .await?;
    Ok(())
}

pub(crate) async fn lock_account(
    connection: &mut PgConnection,
    id: Uuid,
    version: i64,
) -> Result<Account, AdminError> {
    let account = sqlx::query_as::<_, Account>(
        "SELECT id, login, password_hash, auth_version, enabled FROM admin_accounts WHERE id = $1 FOR UPDATE",
    ).bind(id).fetch_optional(connection).await?.ok_or(AdminError::Unauthorized)?;
    if !account.enabled || account.auth_version != version {
        return Err(AdminError::Unauthorized);
    }
    Ok(account)
}

/// Account first, then session: logout/recovery use the same order as hide.
pub(crate) async fn authorize(
    connection: &mut PgConnection,
    proof: &SessionProof,
) -> Result<SessionInfo, AdminError> {
    let account = sqlx::query_as::<_, Account>(
        "SELECT id, login, password_hash, auth_version, enabled FROM admin_accounts WHERE id = $1 FOR SHARE",
    ).bind(proof.account_id).fetch_optional(&mut *connection).await?.ok_or(AdminError::Unauthorized)?;
    if !account.enabled {
        return Err(AdminError::Unauthorized);
    }
    let session: Option<(DateTime<Utc>, String, DateTime<Utc>)> = sqlx::query_as(
        "SELECT expires_at, auth_method, created_at FROM admin_sessions WHERE token_digest = $1 AND account_id = $2
         AND auth_version = $3 AND origin = $4 AND policy_digest = $5 AND revoked_at IS NULL
         AND expires_at > clock_timestamp() AND last_seen_at > clock_timestamp() - interval '15 minutes'
         FOR UPDATE",
    ).bind(proof.token_digest.as_slice()).bind(account.id).bind(account.auth_version)
        .bind(&proof.origin).bind(proof.policy_digest.as_slice()).fetch_optional(&mut *connection).await?;
    let (expires_at, auth_method, created_at) = session.ok_or(AdminError::Unauthorized)?;
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM admin_credentials WHERE account_id = $1 AND revoked_at IS NULL",
    )
    .bind(account.id)
    .fetch_one(&mut *connection)
    .await?;
    if count < 1 {
        return Err(AdminError::Unauthorized);
    }
    if auth_method == "totp" {
        let enabled: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM admin_totp WHERE account_id=$1 AND encrypted_secret IS NOT NULL)")
            .bind(account.id).fetch_one(&mut *connection).await?;
        if !enabled {
            return Err(AdminError::Unauthorized);
        }
    }
    let last_seen_at: DateTime<Utc> = sqlx::query_scalar("UPDATE admin_sessions SET last_seen_at = clock_timestamp() WHERE token_digest = $1 RETURNING last_seen_at")
        .bind(proof.token_digest.as_slice()).fetch_one(connection).await?;
    Ok(SessionInfo {
        account: AccountView {
            id: account.id,
            login: account.login,
        },
        expires_at,
        last_seen_at,
        auth_method,
        created_at,
    })
}

impl AdminService {
    pub(crate) async fn audit_failure(&self, event: &'static str) -> Result<(), AdminError> {
        let mut connection = self.0.pool.acquire().await?;
        audit(&mut connection, None, event).await
    }

    pub(crate) async fn check_password(
        &self,
        login: &str,
        password: Zeroizing<String>,
        peer: IpAddr,
    ) -> Result<Account, AdminError> {
        let login = crypto::login(login).map_err(|_| AdminError::Unauthorized)?;
        let period = Duration::from_secs(300);
        self.0.rate.check("password/global".into(), 60, period)?;
        self.0
            .rate
            .check(format!("password/ip/{peer}"), 12, period)?;
        self.0
            .rate
            .check(format!("password/account/{login}"), 6, period)?;
        if password.len() > 1024 || password.chars().count() < 20 {
            return Err(AdminError::Unauthorized);
        }
        let account = sqlx::query_as::<_, Account>("SELECT id, login, password_hash, auth_version, enabled FROM admin_accounts WHERE login = $1")
            .bind(login).fetch_optional(&self.0.pool).await?;
        let hash = account
            .as_ref()
            .map(|account| account.password_hash.clone())
            .unwrap_or_else(|| self.0.dummy_hash.clone());
        let verified = self
            .crypto(move || crypto::verify_password(&password, &hash))
            .await?;
        let account = account.filter(|account| account.enabled && verified);
        if let Some(account) = account {
            return Ok(account);
        }
        Err(AdminError::Unauthorized)
    }

    pub(crate) async fn options(
        &self,
        headers: &HeaderMap,
    ) -> Result<(OptionsReply, Token), AdminError> {
        if let Some(token) = crypto::cookie(headers, PREAUTH_COOKIE)? {
            let expires: Option<DateTime<Utc>> = sqlx::query_scalar(
                "SELECT expires_at FROM admin_preauth WHERE token_digest = $1 AND origin = $2 AND policy_digest = $3 AND expires_at > clock_timestamp()",
            ).bind(token.digest().as_slice()).bind(&self.0.origin).bind(self.0.policy_digest.as_slice()).fetch_optional(&self.0.pool).await?;
            if let Some(expires_at) = expires {
                return Ok((
                    OptionsReply {
                        csrf_token: token.csrf("preauth", &self.0.origin)?,
                        expires_at,
                        authenticator_policy: "passkey_uv",
                    },
                    token,
                ));
            }
        }
        sqlx::query("DELETE FROM admin_preauth WHERE expires_at <= clock_timestamp()")
            .execute(&self.0.pool)
            .await?;
        let token = Token::random()?;
        let expires_at = sqlx::query_scalar(
            "INSERT INTO admin_preauth(token_digest, origin, policy_digest, expires_at)
             VALUES ($1, $2, $3, clock_timestamp() + interval '5 minutes') RETURNING expires_at",
        )
        .bind(token.digest().as_slice())
        .bind(&self.0.origin)
        .bind(self.0.policy_digest.as_slice())
        .fetch_one(&self.0.pool)
        .await?;
        Ok((
            OptionsReply {
                csrf_token: token.csrf("preauth", &self.0.origin)?,
                expires_at,
                authenticator_policy: "passkey_uv",
            },
            token,
        ))
    }

    pub(crate) async fn preauth(&self, headers: &HeaderMap) -> Result<Token, AdminError> {
        let token = crypto::cookie(headers, PREAUTH_COOKIE)?.ok_or(AdminError::Unauthorized)?;
        token.verify_csrf(headers, "preauth", &self.0.origin)?;
        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM admin_preauth WHERE token_digest = $1 AND origin = $2 AND policy_digest = $3 AND expires_at > clock_timestamp())",
        ).bind(token.digest().as_slice()).bind(&self.0.origin).bind(self.0.policy_digest.as_slice()).fetch_one(&self.0.pool).await?;
        if !exists {
            return Err(AdminError::Unauthorized);
        }
        Ok(token)
    }

    pub(crate) async fn session(
        &self,
        headers: &HeaderMap,
        csrf: bool,
    ) -> Result<(Token, SessionProof, SessionReply), AdminError> {
        let token = crypto::cookie(headers, SESSION_COOKIE)?.ok_or(AdminError::Unauthorized)?;
        if csrf {
            token.verify_csrf(headers, "session", &self.0.origin)?;
        }
        let account_id: Uuid =
            sqlx::query_scalar("SELECT account_id FROM admin_sessions WHERE token_digest = $1")
                .bind(token.digest().as_slice())
                .fetch_optional(&self.0.pool)
                .await?
                .ok_or(AdminError::Unauthorized)?;
        let proof = SessionProof {
            account_id,
            token_digest: token.digest(),
            origin: self.0.origin.clone(),
            policy_digest: self.0.policy_digest,
        };
        let mut transaction = self.0.pool.begin().await?;
        let info = authorize(&mut transaction, &proof).await?;
        transaction.commit().await?;
        let reply = info.reply(&token, &self.0.origin)?;
        Ok((token, proof, reply))
    }

    pub(crate) async fn logout(&self, headers: &HeaderMap) -> Result<(), AdminError> {
        let (_, proof, _) = self.session(headers, true).await?;
        let mut transaction = self.0.pool.begin().await?;
        authorize(&mut transaction, &proof).await?;
        sqlx::query(
            "UPDATE admin_sessions SET revoked_at = clock_timestamp() WHERE token_digest = $1",
        )
        .bind(proof.token_digest.as_slice())
        .execute(&mut *transaction)
        .await?;
        audit(&mut transaction, Some(proof.account_id), "logout").await?;
        transaction.commit().await?;
        Ok(())
    }
}
