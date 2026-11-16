//! Password hashing, opaque tokens and cookie-bound CSRF. No client-side auth state.

use argon2::{
    Algorithm, Argon2, Params, PasswordHash, PasswordHasher, PasswordVerifier, Version,
    password_hash::SaltString,
};
use axum::http::{HeaderMap, header::COOKIE};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use hmac::{Hmac, Mac};
use rand_core::{OsRng, RngCore};
use sha2::{Digest, Sha256};
use zeroize::Zeroize;

use super::AdminError;

pub(crate) const SESSION_COOKIE: &str = "__Host-ed-admin";
pub(crate) const PREAUTH_COOKIE: &str = "__Host-ed-admin-preauth";

pub(crate) struct Token([u8; 32]);
impl Drop for Token {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

impl Token {
    pub fn random() -> Result<Self, AdminError> {
        let mut bytes = [0; 32];
        OsRng
            .try_fill_bytes(&mut bytes)
            .map_err(|_| AdminError::Unavailable)?;
        Ok(Self(bytes))
    }
    pub fn parse(value: &str) -> Result<Self, AdminError> {
        if value.len() != 43 {
            return Err(AdminError::Unauthorized);
        }
        let mut bytes = URL_SAFE_NO_PAD
            .decode(value)
            .map_err(|_| AdminError::Unauthorized)?;
        let token = bytes
            .as_slice()
            .try_into()
            .map(Self)
            .map_err(|_| AdminError::Unauthorized);
        bytes.zeroize();
        token
    }
    pub fn encoded(&self) -> String {
        URL_SAFE_NO_PAD.encode(self.0)
    }
    pub fn digest(&self) -> [u8; 32] {
        Sha256::digest(self.0).into()
    }

    fn mac(&self, purpose: &str, origin: &str) -> Result<Hmac<Sha256>, AdminError> {
        let mut mac =
            Hmac::<Sha256>::new_from_slice(&self.0).map_err(|_| AdminError::Unavailable)?;
        mac.update(b"endlessdrawing/admin/csrf/v1\0");
        mac.update(purpose.as_bytes());
        mac.update(b"\0");
        mac.update(origin.as_bytes());
        Ok(mac)
    }
    pub fn csrf(&self, purpose: &str, origin: &str) -> Result<String, AdminError> {
        Ok(URL_SAFE_NO_PAD.encode(self.mac(purpose, origin)?.finalize().into_bytes()))
    }
    pub fn verify_csrf(
        &self,
        headers: &HeaderMap,
        purpose: &str,
        origin: &str,
    ) -> Result<(), AdminError> {
        let mut values = headers.get_all("x-csrf-token").iter();
        let value = values
            .next()
            .and_then(|value| value.to_str().ok())
            .ok_or(AdminError::Forbidden)?;
        if values.next().is_some() || value.len() != 43 {
            return Err(AdminError::Forbidden);
        }
        let bytes = URL_SAFE_NO_PAD
            .decode(value)
            .map_err(|_| AdminError::Forbidden)?;
        self.mac(purpose, origin)?
            .verify_slice(&bytes)
            .map_err(|_| AdminError::Forbidden)
    }
}

pub(crate) fn cookie(headers: &HeaderMap, name: &str) -> Result<Option<Token>, AdminError> {
    let mut token = None;
    for header in headers.get_all(COOKIE) {
        let value = header.to_str().map_err(|_| AdminError::Unauthorized)?;
        for part in value.split(';') {
            let Some((key, value)) = part.trim().split_once('=') else {
                continue;
            };
            if key == name {
                if token.is_some() {
                    return Err(AdminError::Unauthorized);
                }
                token = Some(Token::parse(value)?);
            }
        }
    }
    Ok(token)
}

pub(crate) fn set_cookie(name: &str, token: &Token, max_age: u32) -> String {
    format!(
        "{name}={}; Path=/; Max-Age={max_age}; HttpOnly; Secure; SameSite=Strict",
        token.encoded()
    )
}
pub(crate) fn clear_cookie(name: &str) -> String {
    format!("{name}=; Path=/; Max-Age=0; HttpOnly; Secure; SameSite=Strict")
}

pub(crate) fn login(value: &str) -> Result<String, AdminError> {
    let value = value.trim().to_ascii_lowercase();
    if !(3..=64).contains(&value.len())
        || !value.as_bytes()[0].is_ascii_alphanumeric()
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_.-".contains(&byte))
    {
        return Err(AdminError::Invalid);
    }
    Ok(value)
}

fn argon() -> Result<Argon2<'static>, AdminError> {
    let params = Params::new(64 * 1024, 3, 1, None).map_err(|_| AdminError::Unavailable)?;
    Ok(Argon2::new(Algorithm::Argon2id, Version::V0x13, params))
}
pub(crate) fn hash_password(password: &str) -> Result<String, AdminError> {
    if password.chars().count() < 20 || password.len() > 1024 {
        return Err(AdminError::Invalid);
    }
    let mut bytes = [0; 16];
    OsRng
        .try_fill_bytes(&mut bytes)
        .map_err(|_| AdminError::Unavailable)?;
    let salt = SaltString::encode_b64(&bytes).map_err(|_| AdminError::Unavailable)?;
    Ok(argon()?
        .hash_password(password.as_bytes(), &salt)
        .map_err(|_| AdminError::Unavailable)?
        .to_string())
}
pub(crate) fn verify_password(password: &str, hash: &str) -> Result<bool, AdminError> {
    let hash = PasswordHash::new(hash).map_err(|_| AdminError::Unavailable)?;
    Ok(argon()?.verify_password(password.as_bytes(), &hash).is_ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn tokens_and_csrf_are_strict_and_bound_to_purpose_origin_and_cookie() {
        let token = Token::random().unwrap();
        let encoded = token.encoded();
        assert_eq!(Token::parse(&encoded).unwrap().digest(), token.digest());
        assert!(Token::parse(&(encoded + "=")).is_err());
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-csrf-token",
            token
                .csrf("session", "https://example.com")
                .unwrap()
                .parse()
                .unwrap(),
        );
        assert!(
            token
                .verify_csrf(&headers, "session", "https://example.com")
                .is_ok()
        );
        assert_eq!(
            token.verify_csrf(&headers, "preauth", "https://example.com"),
            Err(AdminError::Forbidden)
        );
        assert_eq!(
            token.verify_csrf(&headers, "session", "https://other.example.com"),
            Err(AdminError::Forbidden)
        );
        assert_eq!(
            Token::random()
                .unwrap()
                .verify_csrf(&headers, "session", "https://example.com"),
            Err(AdminError::Forbidden)
        );
        headers.append(
            COOKIE,
            format!(
                "{SESSION_COOKIE}={}; {SESSION_COOKIE}={}",
                token.encoded(),
                token.encoded()
            )
            .parse()
            .unwrap(),
        );
        assert!(cookie(&headers, SESSION_COOKIE).is_err());
    }
}
