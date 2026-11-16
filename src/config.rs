//! Startup configuration. Credentials are intentionally not printable via Debug.

use std::{env, net::SocketAddr, path::PathBuf, str::FromStr, time::Duration};

use sqlx::postgres::PgConnectOptions;
use thiserror::Error;

use crate::domain::StrokeLimits;

/// Validated application settings. No Debug implementation: the DSN is sensitive.
pub struct Config {
    /// HTTP listener address; TLS terminates at a reverse proxy in production.
    pub bind_addr: SocketAddr,
    /// Database settings, never log these.
    pub database: PgConnectOptions,
    /// Maximum number of pooled PostgreSQL connections.
    pub database_max_connections: u32,
    /// Maximum wait for a pooled connection and startup connection establishment.
    pub database_acquire_timeout: Duration,
    /// PostgreSQL statement timeout, also used to bound readiness probes.
    pub database_query_timeout: Duration,
    /// Optional Vite build directory. Absent means no frontend is served yet.
    pub static_dir: Option<PathBuf>,
    /// Limits applied when parsing untrusted stroke payloads.
    pub stroke_limits: StrokeLimits,
}

/// A safe configuration error that contains a key, never its secret value.
#[derive(Debug, Error, PartialEq, Eq)]
#[error("invalid or missing configuration: {key}")]
pub struct ConfigError {
    key: &'static str,
}

impl Config {
    /// Reads environment variables without echoing their values on errors.
    ///
    /// # Errors
    /// Returns an error for a missing DSN, malformed or nonpositive settings.
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::from_lookup(|key| env::var(key).ok())
    }

    fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Result<Self, ConfigError> {
        let database = lookup("DATABASE_URL")
            .filter(|value| value.starts_with("postgres://") || value.starts_with("postgresql://"))
            .ok_or(ConfigError {
                key: "DATABASE_URL",
            })?
            .parse()
            .map_err(|_| ConfigError {
                key: "DATABASE_URL",
            })?;
        let bind_addr = setting(&lookup, "BIND_ADDR", "127.0.0.1:3000")?;
        let database_max_connections = positive(&lookup, "DATABASE_MAX_CONNECTIONS", 10_u32)?;
        let acquire_ms = positive(&lookup, "DATABASE_ACQUIRE_TIMEOUT_MS", 5000_u32)?;
        let query_ms = positive(&lookup, "DATABASE_QUERY_TIMEOUT_MS", 5000_u32)?;
        let max_points = positive(&lookup, "STROKE_MAX_POINTS", 2000_usize)?;
        let max_span = setting(&lookup, "STROKE_MAX_SPAN", "100000")?;
        let stroke_limits = StrokeLimits::new(max_points, max_span).map_err(|_| ConfigError {
            key: "STROKE_MAX_SPAN",
        })?;
        let static_dir = lookup("STATIC_DIR")
            .filter(|value| !value.trim().is_empty())
            .map(PathBuf::from);

        Ok(Self {
            bind_addr,
            database,
            database_max_connections,
            database_acquire_timeout: Duration::from_millis(u64::from(acquire_ms)),
            database_query_timeout: Duration::from_millis(u64::from(query_ms)),
            static_dir,
            stroke_limits,
        })
    }
}

fn setting<T: FromStr>(
    lookup: &impl Fn(&str) -> Option<String>,
    key: &'static str,
    default: &str,
) -> Result<T, ConfigError> {
    lookup(key)
        .as_deref()
        .unwrap_or(default)
        .parse()
        .map_err(|_| ConfigError { key })
}

fn positive<T>(
    lookup: &impl Fn(&str) -> Option<String>,
    key: &'static str,
    default: T,
) -> Result<T, ConfigError>
where
    T: FromStr + Default + PartialOrd,
{
    let value = match lookup(key) {
        Some(raw) => raw.parse().map_err(|_| ConfigError { key })?,
        None => default,
    };
    if value <= T::default() {
        return Err(ConfigError { key });
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config_with(overrides: &[(&str, &str)]) -> Result<Config, ConfigError> {
        Config::from_lookup(|key| {
            overrides
                .iter()
                .find(|(name, _)| *name == key)
                .map(|(_, value)| (*value).to_owned())
                .or_else(|| {
                    (key == "DATABASE_URL")
                        .then(|| "postgres://canvas:secret@localhost/canvas".to_owned())
                })
        })
    }

    #[test]
    fn defaults_are_local_and_bounded() {
        let config = config_with(&[]).unwrap();
        assert_eq!(config.bind_addr, "127.0.0.1:3000".parse().unwrap());
        assert_eq!(config.database_max_connections, 10);
        assert_eq!(config.stroke_limits.max_points(), 2000);
        assert!(config.static_dir.is_none());
    }

    #[test]
    fn accepts_explicit_settings() {
        let config = config_with(&[
            ("BIND_ADDR", "0.0.0.0:8080"),
            ("DATABASE_MAX_CONNECTIONS", "2"),
            ("DATABASE_ACQUIRE_TIMEOUT_MS", "100"),
            ("DATABASE_QUERY_TIMEOUT_MS", "200"),
            ("STATIC_DIR", "../frontend/dist"),
            ("STROKE_MAX_POINTS", "40"),
            ("STROKE_MAX_SPAN", "500"),
        ])
        .unwrap();
        assert_eq!(config.bind_addr.port(), 8080);
        assert_eq!(config.database_max_connections, 2);
        assert_eq!(config.database_acquire_timeout, Duration::from_millis(100));
        assert_eq!(config.database_query_timeout, Duration::from_millis(200));
        assert_eq!(config.static_dir, Some(PathBuf::from("../frontend/dist")));
        assert_eq!(config.stroke_limits.max_points(), 40);
        assert_eq!(config.stroke_limits.max_span(), 500.0);
    }

    #[test]
    fn rejects_missing_database_url_without_disclosing_values() {
        let error = Config::from_lookup(|_| None).err().unwrap();
        assert_eq!(error.key, "DATABASE_URL");
        let error = config_with(&[("DATABASE_URL", "secret:not-a-database-url")])
            .err()
            .unwrap();
        assert_eq!(
            error.to_string(),
            "invalid or missing configuration: DATABASE_URL"
        );
        assert!(!format!("{error:?}").contains("secret"));
    }

    #[test]
    fn rejects_invalid_values_without_modifying_process_environment() {
        for (key, value) in [
            ("BIND_ADDR", "not an address"),
            ("DATABASE_MAX_CONNECTIONS", "0"),
            ("DATABASE_ACQUIRE_TIMEOUT_MS", "0"),
            ("DATABASE_QUERY_TIMEOUT_MS", "-1"),
            ("STROKE_MAX_POINTS", "0"),
            ("STROKE_MAX_SPAN", "NaN"),
            ("STROKE_MAX_SPAN", "inf"),
            ("STROKE_MAX_SPAN", "0"),
            ("STROKE_MAX_SPAN", "2000000001"),
        ] {
            assert_eq!(config_with(&[(key, value)]).err().unwrap().key, key);
        }
    }
}
