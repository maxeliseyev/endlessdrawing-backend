//! Owner-approved password + TOTP backup. Secret setup/disable requires fresh UV login.
//! RFC 6238 SHA1/6 digits/30s; +/- one step, atomically consumed to prevent replay.
use super::{
    AdminError, AdminService,
    auth::{self, AccountView, SessionInfo, SessionProof},
    crypto::{self, Token},
};
use aes_gcm::{
    Aes256Gcm, KeyInit,
    aead::{Aead, Payload},
};
use axum::http::HeaderMap;
use chrono::{DateTime, Utc};
use data_encoding::BASE32_NOPAD;
use hmac::{Hmac, Mac};
use rand_core::{OsRng, RngCore};
use serde::{Deserialize, Serialize};
use sha1::Sha1;
use sqlx::{PgConnection, PgPool};
use std::{net::IpAddr, time::Duration};
use subtle::ConstantTimeEq;
use uuid::Uuid;
use zeroize::{Zeroize, Zeroizing};

const FRESH_SECONDS: i64 = 300;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CodeInput {
    code: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LoginInput {
    login: String,
    password: String,
    code: String,
}
#[derive(Serialize)]
pub(crate) struct Status {
    available: bool,
    enabled: bool,
    fresh_passkey: bool,
}
#[derive(Serialize)]
pub(crate) struct Setup {
    secret: String,
    otpauth_uri: String,
    expires_at: DateTime<Utc>,
}

impl Drop for Setup {
    fn drop(&mut self) {
        self.secret.zeroize();
        self.otpauth_uri.zeroize();
    }
}

fn aad(account: Uuid, origin: &str) -> Vec<u8> {
    let mut value = b"endlessdrawing/admin/totp/aes256gcm/v1\0".to_vec();
    value.extend_from_slice(account.as_bytes());
    value.extend_from_slice(origin.as_bytes());
    value
}
fn encrypt(
    key: &[u8; 32],
    account: Uuid,
    origin: &str,
    secret: &[u8],
) -> Result<Vec<u8>, AdminError> {
    let mut nonce = [0; 12];
    OsRng
        .try_fill_bytes(&mut nonce)
        .map_err(|_| AdminError::Unavailable)?;
    let cipher = Aes256Gcm::new_from_slice(key).map_err(|_| AdminError::Unavailable)?;
    let encrypted = cipher
        .encrypt(
            (&nonce).into(),
            Payload {
                msg: secret,
                aad: &aad(account, origin),
            },
        )
        .map_err(|_| AdminError::Unavailable)?;
    let mut result = vec![1];
    result.extend_from_slice(&nonce);
    result.extend_from_slice(&encrypted);
    Ok(result)
}
fn decrypt(
    key: &[u8; 32],
    account: Uuid,
    origin: &str,
    encrypted: &[u8],
) -> Result<Zeroizing<Vec<u8>>, AdminError> {
    if encrypted.len() != 49 || encrypted[0] != 1 {
        return Err(AdminError::Unavailable);
    }
    let cipher = Aes256Gcm::new_from_slice(key).map_err(|_| AdminError::Unavailable)?;
    let nonce: &[u8; 12] = encrypted[1..13]
        .try_into()
        .map_err(|_| AdminError::Unavailable)?;
    cipher
        .decrypt(
            nonce.into(),
            Payload {
                msg: &encrypted[13..],
                aad: &aad(account, origin),
            },
        )
        .map(Zeroizing::new)
        .map_err(|_| AdminError::Unavailable)
}
#[derive(sqlx::FromRow)]
struct EncryptedRow {
    account_id: Uuid,
    encrypted_secret: Option<Vec<u8>>,
    pending_secret: Option<Vec<u8>>,
}

pub(super) async fn validate_key(
    pool: &PgPool,
    key: Option<&[u8; 32]>,
    origin: &str,
) -> Result<(), AdminError> {
    let rows: Vec<EncryptedRow> = sqlx::query_as("SELECT account_id,encrypted_secret,pending_secret FROM admin_totp WHERE encrypted_secret IS NOT NULL OR pending_secret IS NOT NULL")
        .fetch_all(pool).await?;
    for row in rows {
        let key = key.ok_or(AdminError::Unavailable)?;
        for encrypted in [row.encrypted_secret, row.pending_secret]
            .into_iter()
            .flatten()
        {
            decrypt(key, row.account_id, origin, &encrypted)?;
        }
    }
    Ok(())
}
fn code(secret: &[u8], step: i64) -> Result<[u8; 6], AdminError> {
    let mut mac =
        <Hmac<Sha1> as Mac>::new_from_slice(secret).map_err(|_| AdminError::Unavailable)?;
    mac.update(&step.to_be_bytes());
    let digest = mac.finalize().into_bytes();
    let offset = usize::from(digest[19] & 15);
    let truncated = u32::from_be_bytes(
        digest[offset..offset + 4]
            .try_into()
            .map_err(|_| AdminError::Unavailable)?,
    ) & 0x7fff_ffff;
    let mut number = truncated % 1_000_000;
    let mut result = [b'0'; 6];
    for digit in result.iter_mut().rev() {
        *digit += (number % 10) as u8;
        number /= 10;
    }
    Ok(result)
}
fn matching_step(
    secret: &[u8],
    value: &str,
    now: i64,
    last: Option<i64>,
) -> Result<i64, AdminError> {
    if value.len() != 6 || !value.bytes().all(|c| c.is_ascii_digit()) {
        return Err(AdminError::Unauthorized);
    }
    let mut matched = None;
    for step in [now - 1, now, now + 1] {
        let equal = bool::from(code(secret, step)?.as_slice().ct_eq(value.as_bytes()));
        if equal && step >= 0 && last.is_none_or(|last| step > last) {
            matched = Some(step);
        }
    }
    matched.ok_or(AdminError::Unauthorized)
}
async fn db_step(connection: &mut PgConnection) -> Result<i64, AdminError> {
    Ok(
        sqlx::query_scalar("SELECT floor(extract(epoch FROM clock_timestamp())/30)::bigint")
            .fetch_one(connection)
            .await?,
    )
}
async fn fresh(
    connection: &mut PgConnection,
    proof: &SessionProof,
) -> Result<SessionInfo, AdminError> {
    let info = auth::authorize(connection, proof).await?;
    let now: DateTime<Utc> = sqlx::query_scalar("SELECT clock_timestamp()")
        .fetch_one(connection)
        .await?;
    if info.auth_method != "passkey_uv"
        || info.created_at > now + chrono::Duration::seconds(5)
        || (now - info.created_at).num_seconds() >= FRESH_SECONDS
    {
        return Err(AdminError::Forbidden);
    }
    Ok(info)
}
async fn changed(
    connection: &mut PgConnection,
    proof: &SessionProof,
    event: &str,
) -> Result<(), AdminError> {
    let version: i64 = sqlx::query_scalar(
        "UPDATE admin_accounts SET auth_version=auth_version+1 WHERE id=$1 RETURNING auth_version",
    )
    .bind(proof.account_id)
    .fetch_one(&mut *connection)
    .await?;
    sqlx::query("UPDATE admin_sessions SET auth_version=$1 WHERE token_digest=$2")
        .bind(version)
        .bind(proof.token_digest.as_slice())
        .execute(&mut *connection)
        .await?;
    sqlx::query("UPDATE admin_sessions SET revoked_at=clock_timestamp() WHERE account_id=$1 AND token_digest<>$2 AND revoked_at IS NULL")
        .bind(proof.account_id).bind(proof.token_digest.as_slice()).execute(&mut *connection).await?;
    sqlx::query("DELETE FROM admin_preauth WHERE account_id=$1")
        .bind(proof.account_id)
        .execute(&mut *connection)
        .await?;
    sqlx::query("DELETE FROM admin_enrollments WHERE account_id=$1")
        .bind(proof.account_id)
        .execute(&mut *connection)
        .await?;
    auth::audit(connection, Some(proof.account_id), event).await
}
impl AdminService {
    pub(crate) async fn totp_status(&self, headers: &HeaderMap) -> Result<Status, AdminError> {
        let (_, proof, _) = self.session(headers, false).await?;
        let enabled:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM admin_totp WHERE account_id=$1 AND encrypted_secret IS NOT NULL)")
            .bind(proof.account_id).fetch_one(&self.0.pool).await?;
        let recent:bool=sqlx::query_scalar("SELECT auth_method='passkey_uv' AND created_at<=clock_timestamp()+interval '5 seconds' AND created_at>clock_timestamp()-interval '5 minutes' FROM admin_sessions WHERE token_digest=$1")
            .bind(proof.token_digest.as_slice()).fetch_one(&self.0.pool).await?;
        Ok(Status {
            available: self.0.totp_key.is_some(),
            enabled,
            fresh_passkey: recent,
        })
    }
    pub(crate) async fn totp_start(&self, headers: &HeaderMap) -> Result<Setup, AdminError> {
        let key = self.0.totp_key.as_deref().ok_or(AdminError::Unavailable)?;
        let (_, proof, _) = self.session(headers, true).await?;
        self.0.rate.check(
            format!("totp/setup/{}", proof.account_id),
            3,
            Duration::from_secs(300),
        )?;
        let mut tx = self.0.pool.begin().await?;
        // Account EXCLUSIVE before session lock; serializes factor changes with hide/recovery.
        sqlx::query("SELECT id FROM admin_accounts WHERE id=$1 FOR UPDATE")
            .bind(proof.account_id)
            .execute(&mut *tx)
            .await?;
        let info = fresh(&mut tx, &proof).await?;
        let active:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM admin_totp WHERE account_id=$1 AND encrypted_secret IS NOT NULL)")
            .bind(proof.account_id).fetch_one(&mut *tx).await?;
        if active {
            return Err(AdminError::Conflict);
        }
        let mut secret = Zeroizing::new([0; 20]);
        OsRng
            .try_fill_bytes(secret.as_mut())
            .map_err(|_| AdminError::Unavailable)?;
        let encrypted = encrypt(key, proof.account_id, &self.0.origin, secret.as_slice())?;
        let expires_at=sqlx::query_scalar("INSERT INTO admin_totp(account_id,pending_secret,pending_expires_at,pending_session_digest) VALUES($1,$2,LEAST(clock_timestamp()+interval '5 minutes',$4::timestamptz+interval '5 minutes'),$3) ON CONFLICT(account_id) DO UPDATE SET pending_secret=EXCLUDED.pending_secret,pending_expires_at=EXCLUDED.pending_expires_at,pending_session_digest=EXCLUDED.pending_session_digest RETURNING pending_expires_at")
            .bind(proof.account_id).bind(encrypted).bind(proof.token_digest.as_slice()).bind(info.created_at).fetch_one(&mut *tx).await?;
        auth::audit(&mut tx, Some(proof.account_id), "totp_setup_started").await?;
        tx.commit().await?;
        let secret = BASE32_NOPAD.encode(secret.as_slice());
        let mut url = webauthn_rs::prelude::Url::parse("otpauth://totp/")
            .map_err(|_| AdminError::Unavailable)?;
        url.set_path(&format!("Endless Drawing:{}", info.account.login));
        url.query_pairs_mut()
            .append_pair("secret", &secret)
            .append_pair("issuer", "Endless Drawing")
            .append_pair("algorithm", "SHA1")
            .append_pair("digits", "6")
            .append_pair("period", "30");
        Ok(Setup {
            secret,
            otpauth_uri: url.to_string(),
            expires_at,
        })
    }
    pub(crate) async fn totp_confirm(
        &self,
        headers: &HeaderMap,
        input: CodeInput,
    ) -> Result<(), AdminError> {
        let key = self.0.totp_key.as_deref().ok_or(AdminError::Unavailable)?;
        let (_, proof, _) = self.session(headers, true).await?;
        self.0.rate.check(
            format!("totp/confirm/{}", proof.account_id),
            6,
            Duration::from_secs(300),
        )?;
        let mut tx = self.0.pool.begin().await?;
        sqlx::query("SELECT id FROM admin_accounts WHERE id=$1 FOR UPDATE")
            .bind(proof.account_id)
            .execute(&mut *tx)
            .await?;
        fresh(&mut tx, &proof).await?;
        let encrypted:Vec<u8>=sqlx::query_scalar("SELECT pending_secret FROM admin_totp WHERE account_id=$1 AND encrypted_secret IS NULL AND pending_secret IS NOT NULL AND pending_session_digest=$2 AND pending_expires_at>clock_timestamp() FOR UPDATE")
            .bind(proof.account_id).bind(proof.token_digest.as_slice()).fetch_optional(&mut *tx).await?.ok_or(AdminError::Unauthorized)?;
        let secret = decrypt(key, proof.account_id, &self.0.origin, &encrypted)?;
        let step = matching_step(&secret, &input.code, db_step(&mut tx).await?, None)?;
        sqlx::query("UPDATE admin_totp SET encrypted_secret=pending_secret,enabled_at=clock_timestamp(),last_used_step=$1,pending_secret=NULL,pending_expires_at=NULL,pending_session_digest=NULL WHERE account_id=$2")
            .bind(step).bind(proof.account_id).execute(&mut *tx).await?;
        changed(&mut tx, &proof, "totp_enabled").await?;
        tx.commit().await?;
        Ok(())
    }
    pub(crate) async fn totp_disable(&self, headers: &HeaderMap) -> Result<(), AdminError> {
        let (_, proof, _) = self.session(headers, true).await?;
        let mut tx = self.0.pool.begin().await?;
        sqlx::query("SELECT id FROM admin_accounts WHERE id=$1 FOR UPDATE")
            .bind(proof.account_id)
            .execute(&mut *tx)
            .await?;
        fresh(&mut tx, &proof).await?;
        sqlx::query("DELETE FROM admin_totp WHERE account_id=$1")
            .bind(proof.account_id)
            .execute(&mut *tx)
            .await?;
        changed(&mut tx, &proof, "totp_disabled").await?;
        tx.commit().await?;
        Ok(())
    }
    pub(crate) async fn totp_login(
        &self,
        headers: &HeaderMap,
        input: LoginInput,
        peer: IpAddr,
    ) -> Result<(super::auth::SessionReply, Token), AdminError> {
        let pending = self.preauth(headers).await?;
        let account = self
            .check_password(&input.login, Zeroizing::new(input.password), peer)
            .await?;
        let key = self.0.totp_key.as_deref().ok_or(AdminError::Unauthorized)?;
        let mut tx = self.0.pool.begin().await?;
        let account = auth::lock_account(&mut tx, account.id, account.auth_version).await?;
        let count: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM admin_credentials WHERE account_id=$1 AND revoked_at IS NULL",
        )
        .bind(account.id)
        .fetch_one(&mut *tx)
        .await?;
        if count < 1 {
            return Err(AdminError::Unauthorized);
        }
        // Row-lock/consume even across different accounts. A failed code rolls this back;
        // successful concurrent requests cannot reuse one preauth cookie.
        let consumed=sqlx::query("DELETE FROM admin_preauth WHERE token_digest=$1 AND origin=$2 AND policy_digest=$3 AND expires_at>clock_timestamp() AND (account_id IS NULL OR (account_id=$4 AND auth_version=$5))")
            .bind(pending.digest().as_slice()).bind(&self.0.origin).bind(self.0.policy_digest.as_slice()).bind(account.id).bind(account.auth_version).execute(&mut *tx).await?;
        if consumed.rows_affected() != 1 {
            return Err(AdminError::Unauthorized);
        }
        let (encrypted,last):(Vec<u8>,Option<i64>)=sqlx::query_as("SELECT encrypted_secret,last_used_step FROM admin_totp WHERE account_id=$1 AND encrypted_secret IS NOT NULL FOR UPDATE")
            .bind(account.id).fetch_optional(&mut *tx).await?.ok_or(AdminError::Unauthorized)?;
        let secret = decrypt(key, account.id, &self.0.origin, &encrypted)?;
        let step = matching_step(&secret, &input.code, db_step(&mut tx).await?, last)?;
        sqlx::query("UPDATE admin_totp SET last_used_step=$1 WHERE account_id=$2")
            .bind(step)
            .bind(account.id)
            .execute(&mut *tx)
            .await?;
        let token = Token::random()?;
        let (expires_at,last_seen_at):(DateTime<Utc>,DateTime<Utc>)=sqlx::query_as("INSERT INTO admin_sessions(token_digest,account_id,auth_version,origin,policy_digest,auth_method,expires_at) VALUES($1,$2,$3,$4,$5,'totp',clock_timestamp()+interval '8 hours') RETURNING expires_at,last_seen_at")
            .bind(token.digest().as_slice()).bind(account.id).bind(account.auth_version).bind(&self.0.origin).bind(self.0.policy_digest.as_slice()).fetch_one(&mut *tx).await?;
        if let Ok(Some(old)) = crypto::cookie(headers, crypto::SESSION_COOKIE) {
            sqlx::query(
                "UPDATE admin_sessions SET revoked_at=clock_timestamp() WHERE token_digest=$1",
            )
            .bind(old.digest().as_slice())
            .execute(&mut *tx)
            .await?;
        }
        auth::audit(&mut tx, Some(account.id), "login_totp_succeeded").await?;
        tx.commit().await?;
        let info = SessionInfo {
            account: AccountView {
                id: account.id,
                login: account.login,
            },
            expires_at,
            last_seen_at,
            created_at: last_seen_at,
            auth_method: "totp".into(),
        };
        Ok((info.reply(&token, &self.0.origin)?, token))
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rfc6238_and_replay_skew_and_exact_digits() {
        let secret = b"12345678901234567890";
        for (time, expected) in [
            (59, "287082"),
            (1111111109, "081804"),
            (1111111111, "050471"),
            (1234567890, "005924"),
            (2000000000, "279037"),
            (20000000000, "353130"),
        ] {
            let actual = code(secret, time / 30).unwrap();
            assert_eq!(actual.as_slice(), expected.as_bytes());
            assert!(matching_step(secret, expected, time / 30, Some(time / 30)).is_err());
            assert_eq!(
                matching_step(secret, expected, time / 30 - 1, None).unwrap(),
                time / 30
            );
            assert!(matching_step(secret, expected, time / 30 - 2, None).is_err());
        }
        for invalid in ["12345", "1234567", "１２３４５６", "12 345", "abcdef"] {
            assert!(matching_step(secret, invalid, 10, None).is_err());
        }
    }
    #[test]
    fn envelopes_are_random_authenticated_and_account_origin_key_bound() {
        let id = Uuid::new_v4();
        let key = [7; 32];
        let secret = [8; 20];
        let value = encrypt(&key, id, "https://example.test", &secret).unwrap();
        assert_eq!(value.len(), 49);
        assert_eq!(
            decrypt(&key, id, "https://example.test", &value)
                .unwrap()
                .as_slice(),
            secret
        );
        assert_ne!(
            value,
            encrypt(&key, id, "https://example.test", &secret).unwrap()
        );
        assert!(decrypt(&[9; 32], id, "https://example.test", &value).is_err());
        assert!(decrypt(&key, Uuid::new_v4(), "https://example.test", &value).is_err());
        assert!(decrypt(&key, id, "https://evil.test", &value).is_err());
        let mut bad = value;
        bad[20] ^= 1;
        assert!(decrypt(&key, id, "https://example.test", &bad).is_err());
    }
}
