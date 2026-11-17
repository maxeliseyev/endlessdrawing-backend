//! Password-gated, single-use WebAuthn ceremonies; only verified assertions mint sessions.

use axum::http::HeaderMap;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, types::Json};
use std::net::IpAddr;
use uuid::Uuid;
use webauthn_rs::prelude::*;
use zeroize::Zeroizing;

use super::{
    AdminError, AdminService,
    auth::{self, Account, AccountView, CredentialRow, SessionInfo, SessionReply},
    crypto::Token,
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LoginInput {
    pub login: String,
    pub password: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LoginVerify {
    pub credential: PublicKeyCredential,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct EnrollmentInput {
    pub login: String,
    pub password: String,
    pub enrollment_token: String,
    pub label: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct EnrollmentFinish {
    pub credential: RegisterPublicKeyCredential,
}

#[derive(Serialize)]
pub(crate) struct LoginChallenge {
    status: &'static str,
    webauthn: RequestChallengeResponse,
    expires_at: DateTime<Utc>,
}
#[derive(Serialize)]
pub(crate) struct EnrollmentChallenge {
    webauthn: CreationChallengeResponse,
    expires_at: DateTime<Utc>,
}
#[derive(Serialize)]
pub(crate) struct EnrollmentResult {
    registered_credentials: i64,
    required_credentials: i64,
    complete: bool,
}

#[derive(FromRow)]
struct Claim {
    account_id: Uuid,
    auth_version: i64,
    challenge_state: Json<serde_json::Value>,
    enrollment_digest: Option<Vec<u8>>,
    label: Option<String>,
}

impl AdminService {
    pub(crate) async fn login_start(
        &self,
        headers: &HeaderMap,
        input: LoginInput,
        peer: IpAddr,
    ) -> Result<(LoginChallenge, Token), AdminError> {
        let token = self.preauth(headers).await?;
        let account = self
            .check_password(&input.login, Zeroizing::new(input.password), peer)
            .await?;
        let rows = self.credentials(account.id).await?;
        if rows.is_empty() {
            return Err(AdminError::Unauthorized);
        }
        let inner = self.0.clone();
        let (webauthn, state) = self
            .crypto(move || {
                let keys: Vec<_> = rows.into_iter().map(|row| row.credential.0).collect();
                inner
                    .webauthn
                    .start_passkey_authentication(&keys)
                    .map_err(|_| AdminError::Unauthorized)
            })
            .await?;
        let expires_at = self
            .set_challenge(
                &token,
                &account,
                "login",
                serde_json::to_value(state).map_err(|_| AdminError::Unavailable)?,
                None,
                None,
            )
            .await?;
        Ok((
            LoginChallenge {
                status: "mfa_required",
                webauthn,
                expires_at,
            },
            token,
        ))
    }

    pub(crate) async fn login_finish(
        &self,
        headers: &HeaderMap,
        input: LoginVerify,
    ) -> Result<(SessionReply, Token), AdminError> {
        let pending = self.preauth(headers).await?;
        let claim = self.take_challenge(&pending, "login").await?;
        if input.credential.type_ != "public-key"
            || URL_SAFE_NO_PAD.encode(&input.credential.raw_id) != input.credential.id
            || input
                .credential
                .get_user_unique_id()
                .is_some_and(|id| id != claim.account_id.as_bytes())
        {
            return Err(AdminError::Unauthorized);
        }
        let state: PasskeyAuthentication =
            serde_json::from_value(claim.challenge_state.0).map_err(|_| AdminError::Unavailable)?;
        let inner = self.0.clone();
        let result = self
            .crypto(move || {
                inner
                    .webauthn
                    .finish_passkey_authentication(&input.credential, &state)
                    .map_err(|_| AdminError::Unauthorized)
            })
            .await?;
        if !result.user_verified() {
            return Err(AdminError::Unauthorized);
        }
        let mut transaction = self.0.pool.begin().await?;
        let account =
            auth::lock_account(&mut transaction, claim.account_id, claim.auth_version).await?;
        let count: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM admin_credentials WHERE account_id = $1 AND revoked_at IS NULL",
        )
        .bind(account.id)
        .fetch_one(&mut *transaction)
        .await?;
        if count < 1 {
            return Err(AdminError::Unauthorized);
        }
        let row = sqlx::query_as::<_, CredentialRow>(
            "SELECT credential_id, credential, counter FROM admin_credentials WHERE credential_id = $1 AND account_id = $2 AND revoked_at IS NULL FOR UPDATE",
        ).bind(result.cred_id()).bind(account.id).fetch_optional(&mut *transaction).await?.ok_or(AdminError::Unauthorized)?;
        let mut key = row.credential.0;
        // Compare with the CURRENT DB counter, not only the ceremony's earlier snapshot.
        let counter = i64::from(result.counter());
        // Synced passkeys may have zero/non-global counters. Their replay protection is
        // the single-use challenge; retain strict monotonic checks for device-bound keys.
        if !result.backup_eligible() && (counter != 0 || row.counter != 0) && counter <= row.counter
        {
            return Err(AdminError::Unauthorized);
        }
        if key.update_credential(&result).is_none() {
            return Err(AdminError::Unauthorized);
        }
        sqlx::query(
            "UPDATE admin_credentials SET credential = $1, counter = $2 WHERE credential_id = $3",
        )
        .bind(Json(key))
        .bind(counter.max(row.counter))
        .bind(row.credential_id)
        .execute(&mut *transaction)
        .await?;
        let token = Token::random()?;
        let (expires_at, last_seen_at): (DateTime<Utc>, DateTime<Utc>) = sqlx::query_as(
            "INSERT INTO admin_sessions(token_digest, account_id, auth_version, origin, policy_digest, expires_at)
             VALUES ($1, $2, $3, $4, $5, clock_timestamp() + interval '8 hours') RETURNING expires_at, last_seen_at",
        ).bind(token.digest().as_slice()).bind(account.id).bind(account.auth_version).bind(&self.0.origin)
            .bind(self.0.policy_digest.as_slice()).fetch_one(&mut *transaction).await?;
        // Revoke any full session presented during a new login (session rotation).
        if let Ok(Some(old)) = super::crypto::cookie(headers, super::crypto::SESSION_COOKIE) {
            sqlx::query(
                "UPDATE admin_sessions SET revoked_at = clock_timestamp() WHERE token_digest = $1",
            )
            .bind(old.digest().as_slice())
            .execute(&mut *transaction)
            .await?;
        }
        sqlx::query("DELETE FROM admin_preauth WHERE token_digest = $1")
            .bind(pending.digest().as_slice())
            .execute(&mut *transaction)
            .await?;
        auth::audit(&mut transaction, Some(account.id), "login_succeeded").await?;
        transaction.commit().await?;
        let info = SessionInfo {
            account: AccountView {
                id: account.id,
                login: account.login,
            },
            expires_at,
            last_seen_at,
            created_at: last_seen_at,
            auth_method: "passkey_uv".into(),
        };
        Ok((info.reply(&token, &self.0.origin)?, token))
    }

    pub(crate) async fn enrollment_start(
        &self,
        headers: &HeaderMap,
        input: EnrollmentInput,
        peer: IpAddr,
    ) -> Result<(EnrollmentChallenge, Token), AdminError> {
        let token = self.preauth(headers).await?;
        let label = input.label.trim().to_owned();
        if label.is_empty() || label.chars().count() > 64 || label.len() > 256 {
            return Err(AdminError::Invalid);
        }
        let ticket = Token::parse(&input.enrollment_token)?;
        let account = self
            .check_password(&input.login, Zeroizing::new(input.password), peer)
            .await?;
        let valid: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM admin_enrollments WHERE token_digest = $1 AND account_id = $2 AND auth_version = $3 AND remaining > 0 AND expires_at > clock_timestamp())",
        ).bind(ticket.digest().as_slice()).bind(account.id).bind(account.auth_version).fetch_one(&self.0.pool).await?;
        if !valid {
            return Err(AdminError::Unauthorized);
        }
        let rows = self.credentials(account.id).await?;
        if rows.len() >= 2 {
            return Err(AdminError::Unauthorized);
        }
        let excluded = rows.into_iter().map(|row| row.credential_id).collect();
        let inner = self.0.clone();
        let id = account.id;
        let login = account.login.clone();
        let (webauthn, state) = self
            .crypto(move || {
                inner
                    .webauthn
                    .start_passkey_registration(id, &login, &login, Some(excluded))
                    .map_err(|_| AdminError::Unauthorized)
            })
            .await?;
        let expires_at = self
            .set_challenge(
                &token,
                &account,
                "enrollment",
                serde_json::to_value(state).map_err(|_| AdminError::Unavailable)?,
                Some(ticket.digest().to_vec()),
                Some(label),
            )
            .await?;
        Ok((
            EnrollmentChallenge {
                webauthn,
                expires_at,
            },
            token,
        ))
    }

    pub(crate) async fn enrollment_finish(
        &self,
        headers: &HeaderMap,
        input: EnrollmentFinish,
    ) -> Result<EnrollmentResult, AdminError> {
        let token = self.preauth(headers).await?;
        let claim = self.take_challenge(&token, "enrollment").await?;
        if input.credential.type_ != "public-key"
            || URL_SAFE_NO_PAD.encode(&input.credential.raw_id) != input.credential.id
        {
            return Err(AdminError::Unauthorized);
        }
        let state: PasskeyRegistration =
            serde_json::from_value(claim.challenge_state.0).map_err(|_| AdminError::Unavailable)?;
        let inner = self.0.clone();
        let key = self
            .crypto(move || {
                inner
                    .webauthn
                    .finish_passkey_registration(&input.credential, &state)
                    .map_err(|_| AdminError::Unauthorized)
            })
            .await?;
        let internals: Credential = key.clone().into();
        if !internals.user_verified {
            return Err(AdminError::Unauthorized);
        }
        let mut transaction = self.0.pool.begin().await?;
        let account =
            auth::lock_account(&mut transaction, claim.account_id, claim.auth_version).await?;
        let count: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM admin_credentials WHERE account_id = $1 AND revoked_at IS NULL",
        )
        .bind(account.id)
        .fetch_one(&mut *transaction)
        .await?;
        if count >= 2 {
            return Err(AdminError::Unauthorized);
        }
        let used = sqlx::query(
            "UPDATE admin_enrollments SET remaining = remaining - 1 WHERE token_digest = $1 AND account_id = $2 AND auth_version = $3 AND remaining > 0 AND expires_at > clock_timestamp()",
        ).bind(claim.enrollment_digest.ok_or(AdminError::Unauthorized)?).bind(account.id).bind(account.auth_version).execute(&mut *transaction).await?;
        if used.rows_affected() != 1 {
            return Err(AdminError::Unauthorized);
        }
        let inserted = sqlx::query(
            "INSERT INTO admin_credentials(credential_id, account_id, credential, counter, label)
             VALUES ($1, $2, $3, $4, $5) ON CONFLICT (credential_id) DO NOTHING",
        )
        .bind(key.cred_id())
        .bind(account.id)
        .bind(Json(&key))
        .bind(i64::from(internals.counter))
        .bind(claim.label.ok_or(AdminError::Unauthorized)?)
        .execute(&mut *transaction)
        .await?;
        if inserted.rows_affected() != 1 {
            return Err(AdminError::Unauthorized);
        }
        auth::audit(&mut transaction, Some(account.id), "credential_registered").await?;
        transaction.commit().await?;
        Ok(EnrollmentResult {
            registered_credentials: count + 1,
            required_credentials: 1,
            complete: true,
        })
    }

    async fn credentials(&self, account: Uuid) -> Result<Vec<CredentialRow>, AdminError> {
        Ok(sqlx::query_as("SELECT credential_id, credential, counter FROM admin_credentials WHERE account_id = $1 AND revoked_at IS NULL ORDER BY created_at")
            .bind(account).fetch_all(&self.0.pool).await?)
    }

    async fn set_challenge(
        &self,
        token: &Token,
        account: &Account,
        kind: &str,
        state: serde_json::Value,
        ticket: Option<Vec<u8>>,
        label: Option<String>,
    ) -> Result<DateTime<Utc>, AdminError> {
        sqlx::query_scalar(
            "UPDATE admin_preauth SET kind = $1, account_id = $2, auth_version = $3, challenge_state = $4,
             challenge_expires_at = clock_timestamp() + interval '120 seconds', enrollment_digest = $5, label = $6,
             expires_at = clock_timestamp() + interval '5 minutes'
             WHERE token_digest = $7 AND origin = $8 AND policy_digest = $9 AND expires_at > clock_timestamp()
             RETURNING challenge_expires_at",
        ).bind(kind).bind(account.id).bind(account.auth_version).bind(Json(state)).bind(ticket).bind(label)
            .bind(token.digest().as_slice()).bind(&self.0.origin).bind(self.0.policy_digest.as_slice())
            .fetch_optional(&self.0.pool).await?.ok_or(AdminError::Unauthorized)
    }

    async fn take_challenge(&self, token: &Token, kind: &str) -> Result<Claim, AdminError> {
        let mut transaction = self.0.pool.begin().await?;
        let claim = sqlx::query_as::<_, Claim>(
            "SELECT account_id, auth_version, challenge_state, enrollment_digest, label FROM admin_preauth
             WHERE token_digest = $1 AND origin = $2 AND policy_digest = $3 AND kind = $4
             AND expires_at > clock_timestamp() AND challenge_expires_at > clock_timestamp() AND challenge_state IS NOT NULL FOR UPDATE",
        ).bind(token.digest().as_slice()).bind(&self.0.origin).bind(self.0.policy_digest.as_slice()).bind(kind)
            .fetch_optional(&mut *transaction).await?.ok_or(AdminError::Unauthorized)?;
        sqlx::query("UPDATE admin_preauth SET kind = NULL, account_id = NULL, auth_version = NULL, challenge_state = NULL, challenge_expires_at = NULL, enrollment_digest = NULL, label = NULL WHERE token_digest = $1")
            .bind(token.digest().as_slice()).execute(&mut *transaction).await?;
        // Consumption is committed even if cryptographic verification subsequently fails.
        transaction.commit().await?;
        Ok(claim)
    }
}
