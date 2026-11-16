//! Explicit, fail-closed administrator configuration for password + UV-required passkeys.

use webauthn_rs::prelude::{Url, WebauthnBuilder};

use super::AdminError;

/// Administrator RP configuration. Credentials are bound to this exact HTTPS origin.
#[derive(Clone)]
pub struct AdminConfig {
    pub(crate) origin: Url,
    pub(crate) rp_id: String,
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
        Ok(Self { origin: url, rp_id })
    }
}
