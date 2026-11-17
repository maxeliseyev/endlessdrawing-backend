//! Separate administrator authentication. Anonymous drawing authorization is unchanged.

mod api;
mod auth;
mod ceremony;
mod config;
mod crypto;
mod error;
mod moderation;
pub mod provision;
mod rate;
mod totp;

pub use config::AdminConfig;
pub use error::AdminError;

use axum::Router;
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use std::{path::PathBuf, sync::Arc, time::Duration};
use tokio::sync::Semaphore;
use webauthn_rs::prelude::{Webauthn, WebauthnBuilder};

use crate::realtime::{Realtime, Writer};
use crypto::Token;
use rate::RateLimits;

/// Administrator service: password + UV passkey, or explicitly configured TOTP backup.
#[derive(Clone)]
pub struct AdminService(Arc<Inner>);
struct Inner {
    pool: PgPool,
    writer: Writer,
    origin: String,
    webauthn: Webauthn,
    policy_digest: [u8; 32],
    dummy_hash: String,
    crypto_jobs: Arc<Semaphore>,
    rate: RateLimits,
    totp_key: Option<zeroize::Zeroizing<[u8; 32]>>,
}

impl AdminService {
    /// Initializes password + WebAuthn passkeys, including Touch ID and synced providers.
    ///
    /// # Errors
    /// Fails closed on invalid RP/origin settings or crypto errors.
    pub async fn new(
        pool: PgPool,
        realtime: &Realtime,
        mut config: AdminConfig,
    ) -> Result<Self, AdminError> {
        // Bind saved sessions/ceremonies to this policy and RP as well as the exact origin.
        let mut policy = Sha256::new();
        policy.update(b"endlessdrawing/admin/password+(passkey-uv|totp-backup)/v2\0");
        policy.update(config.rp_id.as_bytes());
        let policy_digest = policy.finalize().into();
        let webauthn = WebauthnBuilder::new(&config.rp_id, &config.origin)
            .map_err(|_| AdminError::Unavailable)?
            .rp_name("Endless Drawing administration")
            .timeout(Duration::from_secs(120))
            .build()
            .map_err(|_| AdminError::Unavailable)?;
        if let Some(path) = config.totp_key_file.take() {
            let metadata = tokio::fs::symlink_metadata(&path)
                .await
                .map_err(|_| AdminError::Unavailable)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if metadata.permissions().mode() & 0o077 != 0 {
                    return Err(AdminError::Unavailable);
                }
            }
            if !metadata.is_file() || metadata.len() != 32 {
                return Err(AdminError::Unavailable);
            }
            let bytes = zeroize::Zeroizing::new(
                tokio::fs::read(path)
                    .await
                    .map_err(|_| AdminError::Unavailable)?,
            );
            config.totp_key = Some(zeroize::Zeroizing::new(
                bytes
                    .as_slice()
                    .try_into()
                    .map_err(|_| AdminError::Unavailable)?,
            ));
        }
        totp::validate_key(
            &pool,
            config.totp_key.as_deref(),
            &config.origin.origin().ascii_serialization(),
        )
        .await?;
        let dummy_password = Token::random()?.encoded();
        let dummy_hash =
            tokio::task::spawn_blocking(move || crypto::hash_password(&dummy_password))
                .await
                .map_err(|_| AdminError::Unavailable)??;
        Ok(Self(Arc::new(Inner {
            pool,
            writer: realtime.writer.clone(),
            origin: config.origin.origin().ascii_serialization(),
            webauthn,
            policy_digest,
            dummy_hash,
            crypto_jobs: Arc::new(Semaphore::new(2)),
            rate: RateLimits::default(),
            totp_key: config.totp_key,
        })))
    }

    async fn crypto<T: Send + 'static>(
        &self,
        work: impl FnOnce() -> Result<T, AdminError> + Send + 'static,
    ) -> Result<T, AdminError> {
        let permit = self
            .0
            .crypto_jobs
            .clone()
            .try_acquire_owned()
            .map_err(|_| AdminError::RateLimited)?;
        // The permit remains in the blocking job even if the request disconnects.
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            work()
        })
        .await
        .map_err(|_| AdminError::Unavailable)?
    }
}

/// Adds isolated admin API/static-entry routes. None means explicit admin-unavailable responses.
pub fn router(service: Option<AdminService>, static_dir: Option<PathBuf>) -> Router {
    api::router(service, static_dir)
}

pub(crate) use auth::SessionProof;
pub(crate) use moderation::{HideResult, ValidatedHide, execute_hide};
