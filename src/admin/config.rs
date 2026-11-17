//! Explicit, fail-closed administrator configuration for password + UV-required passkeys.

use std::path::PathBuf;
use webauthn_rs::prelude::{Url, WebauthnBuilder};
use zeroize::Zeroizing;

use super::AdminError;

/// Administrator RP configuration. Credentials are bound to this exact HTTPS origin.
#[derive(Clone)]
pub struct AdminConfig {
    pub(crate) origin: Url,
    pub(crate) rp_id: String,
    pub(crate) totp_key: Option<Zeroizing<[u8; 32]>>,
    pub(crate) totp_key_file: Option<PathBuf>,
}

impl AdminConfig {
    /// Constructs a strict HTTPS configuration for platform or roaming passkeys.
    ///
    /// # Errors
    /// Rejects noncanonical/non-HTTPS origins or an invalid RP ID.
    pub fn new(origin: &str, rp_id: String) -> Result<Self, AdminError> {
        let url = Url::parse(origin).map_err(|_| AdminError::Unavailable)?;
        if url.scheme() != "https"
            || url.origin().ascii_serialization() != origin
            || !url.username().is_empty()
            || url.password().is_some()
            || rp_id.is_empty()
        {
            return Err(AdminError::Unavailable);
        }
        WebauthnBuilder::new(&rp_id, &url).map_err(|_| AdminError::Unavailable)?;
        Ok(Self {
            origin: url,
            rp_id,
            totp_key: None,
            totp_key_file: None,
        })
    }

    /// Supplies an in-memory encryption key (for embedded/test deployments). Never log it.
    #[must_use]
    pub fn with_totp_key(mut self, key: [u8; 32]) -> Self {
        self.totp_key = Some(Zeroizing::new(key));
        self
    }

    /// Reads a private, raw 32-byte key at service startup, never from frontend/env values.
    #[must_use]
    pub fn with_totp_key_file(mut self, path: PathBuf) -> Self {
        self.totp_key_file = Some(path);
        self
    }
}
