//! Startup configuration. Credentials are intentionally not printable via Debug.

use std::{env, net::SocketAddr, path::PathBuf, str::FromStr, time::Duration};

use sqlx::postgres::PgConnectOptions;
use thiserror::Error;

use crate::admin::AdminConfig;
use crate::domain::{StrokeLimits, WORLD_LIMIT};
pub use crate::realtime_settings::RealtimeSettings;

/// Validated application settings. No Debug implementation: the DSN is sensitive.
pub struct Config {
    /// HTTP listener address; TLS terminates at a reverse proxy in production.
    pub bind_addr: SocketAddr,
    /// Database settings, never log these.
    pub database: PgConnectOptions,
    /// Development convenience only; production runs the separate maintenance migrator.
    pub database_migrate_on_start: bool,
    /// Fail closed if the production runtime role has DDL/cluster privileges or memberships.
    pub database_require_runtime_role: bool,
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
    /// WebSocket protocol, resource limits and allowed browser origins.
    pub realtime: RealtimeSettings,
    /// Disabled unless explicitly configured with HTTPS RP/origin for UV-required passkeys.
    pub admin: Option<AdminConfig>,
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
        let database_migrate_on_start = setting(&lookup, "DATABASE_MIGRATE_ON_START", "true")?;
        let database_require_runtime_role =
            setting(&lookup, "DATABASE_REQUIRE_RUNTIME_ROLE", "false")?;
        if database_require_runtime_role && database_migrate_on_start {
            return Err(ConfigError {
                key: "DATABASE_MIGRATE_ON_START",
            });
        }
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

        let admin = if setting::<bool>(&lookup, "ADMIN_ENABLED", "false")? {
            let required = |key| {
                lookup(key)
                    .filter(|value| !value.is_empty())
                    .ok_or(ConfigError { key })
            };
            let mut config = AdminConfig::new(&required("ADMIN_ORIGIN")?, required("ADMIN_RP_ID")?)
                .map_err(|_| ConfigError {
                    key: "ADMIN_CONFIG",
                })?;
            if let Some(path) = lookup("ADMIN_TOTP_KEY_FILE").filter(|value| !value.is_empty()) {
                config = config.with_totp_key_file(path.into());
            }
            Some(config)
        } else {
            None
        };

        Ok(Self {
            bind_addr,
            database,
            database_migrate_on_start,
            database_require_runtime_role,
            database_max_connections,
            database_acquire_timeout: Duration::from_millis(u64::from(acquire_ms)),
            database_query_timeout: Duration::from_millis(u64::from(query_ms)),
            static_dir,
            stroke_limits,
            realtime: RealtimeSettings::from_lookup(&lookup)?,
            admin,
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

impl RealtimeSettings {
    fn from_lookup(lookup: &impl Fn(&str) -> Option<String>) -> Result<Self, ConfigError> {
        let mut settings = Self {
            msgpack_enabled: setting(lookup, "WS_MSGPACK_ENABLED", "true")?,
            manifest_enabled: setting(lookup, "WS_MANIFEST_ENABLED", "true")?,
            ..Self::default()
        };
        if let Some(origins) = lookup("ALLOWED_ORIGINS") {
            settings.allowed_origins = origins
                .split(',')
                .map(str::trim)
                .map(str::to_owned)
                .collect();
        }
        for (key, target) in [
            ("WS_MAX_CONNECTIONS", &mut settings.max_connections),
            ("WS_MAX_MESSAGE_BYTES", &mut settings.max_message_bytes),
            (
                "MUTATION_QUEUE_CAPACITY",
                &mut settings.mutation_queue_capacity,
            ),
            ("EVENT_BUFFER_CAPACITY", &mut settings.event_buffer_capacity),
            ("SNAPSHOT_CHUNK_BYTES", &mut settings.snapshot_chunk_bytes),
            ("WS_CREATES_PER_SECOND", &mut settings.creates_per_second),
            ("WS_DELETES_PER_SECOND", &mut settings.deletes_per_second),
            (
                "WS_SUBSCRIBES_PER_SECOND",
                &mut settings.subscribes_per_second,
            ),
            ("WS_MESSAGES_PER_SECOND", &mut settings.messages_per_second),
        ] {
            *target = positive(lookup, key, *target)?;
        }
        settings.snapshot_page_size =
            positive(lookup, "SNAPSHOT_PAGE_SIZE", settings.snapshot_page_size)?;
        settings.viewport_max_span = setting(lookup, "VIEWPORT_MAX_SPAN", "30000")?;
        for (key, target) in [
            ("WS_HELLO_TIMEOUT_MS", &mut settings.hello_timeout),
            ("WS_SEND_TIMEOUT_MS", &mut settings.send_timeout),
            ("WS_OPERATION_TIMEOUT_MS", &mut settings.operation_timeout),
            ("SNAPSHOT_TIMEOUT_MS", &mut settings.snapshot_timeout),
            ("WS_HEARTBEAT_INTERVAL_MS", &mut settings.heartbeat_interval),
            ("WS_IDLE_TIMEOUT_MS", &mut settings.idle_timeout),
            ("SPAWN_REFRESH_MS", &mut settings.spawn_refresh),
        ] {
            let millis = positive(lookup, key, target.as_millis() as u64)?;
            *target = Duration::from_millis(millis);
        }
        settings.validate()?;
        Ok(settings)
    }

    /// Validates settings supplied either programmatically or through the environment.
    ///
    /// # Errors
    /// Rejects invalid origins, zero/oversized capacities, ranges and timeouts.
    pub fn validate(&self) -> Result<(), ConfigError> {
        let invalid = |key| ConfigError { key };
        if self.allowed_origins.is_empty()
            || self.allowed_origins.iter().any(|origin| {
                let Ok(uri) = origin.parse::<axum::http::Uri>() else {
                    return true;
                };
                !matches!(uri.scheme_str(), Some("http" | "https"))
                    || uri.authority().is_none()
                    || origin.contains('@')
                    || format!(
                        "{}://{}",
                        uri.scheme_str().unwrap_or_default(),
                        uri.authority().map(|a| a.as_str()).unwrap_or_default()
                    ) != *origin
            })
        {
            return Err(invalid("ALLOWED_ORIGINS"));
        }
        for (key, value, max) in [
            ("WS_MAX_CONNECTIONS", self.max_connections, 65_536),
            ("WS_MAX_MESSAGE_BYTES", self.max_message_bytes, 1024 * 1024),
            (
                "MUTATION_QUEUE_CAPACITY",
                self.mutation_queue_capacity,
                65_536,
            ),
            ("EVENT_BUFFER_CAPACITY", self.event_buffer_capacity, 65_536),
            (
                "SNAPSHOT_CHUNK_BYTES",
                self.snapshot_chunk_bytes,
                4 * 1024 * 1024,
            ),
            ("WS_CREATES_PER_SECOND", self.creates_per_second, 65_536),
            ("WS_DELETES_PER_SECOND", self.deletes_per_second, 65_536),
            (
                "WS_SUBSCRIBES_PER_SECOND",
                self.subscribes_per_second,
                65_536,
            ),
            ("WS_MESSAGES_PER_SECOND", self.messages_per_second, 65_536),
        ] {
            if value == 0 || value > max {
                return Err(invalid(key));
            }
        }
        if self.snapshot_chunk_bytes < 1024 {
            return Err(invalid("SNAPSHOT_CHUNK_BYTES"));
        }
        if self.snapshot_page_size == 0 || self.snapshot_page_size > 1000 {
            return Err(invalid("SNAPSHOT_PAGE_SIZE"));
        }
        if !self.viewport_max_span.is_finite()
            || self.viewport_max_span <= 0.0
            || self.viewport_max_span > 2.0 * WORLD_LIMIT
        {
            return Err(invalid("VIEWPORT_MAX_SPAN"));
        }
        for (key, timeout) in [
            ("WS_HELLO_TIMEOUT_MS", self.hello_timeout),
            ("WS_SEND_TIMEOUT_MS", self.send_timeout),
            ("WS_OPERATION_TIMEOUT_MS", self.operation_timeout),
            ("SNAPSHOT_TIMEOUT_MS", self.snapshot_timeout),
            ("WS_HEARTBEAT_INTERVAL_MS", self.heartbeat_interval),
            ("WS_IDLE_TIMEOUT_MS", self.idle_timeout),
            ("SPAWN_REFRESH_MS", self.spawn_refresh),
        ] {
            if timeout.is_zero() || timeout > Duration::from_secs(3600) {
                return Err(invalid(key));
            }
        }
        if self.heartbeat_interval >= self.idle_timeout {
            return Err(invalid("WS_IDLE_TIMEOUT_MS"));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messagepack_rollout_gate_is_strict_and_can_select_json_only() {
        assert!(config_with(&[]).unwrap().realtime.msgpack_enabled);
        assert!(
            !config_with(&[("WS_MSGPACK_ENABLED", "false")])
                .unwrap()
                .realtime
                .msgpack_enabled
        );
        assert!(config_with(&[("WS_MSGPACK_ENABLED", "yes")]).is_err());
    }

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
    fn restricted_runtime_requires_read_only_startup_and_strict_boolean_settings() {
        let local = config_with(&[]).unwrap();
        assert!(local.database_migrate_on_start);
        assert!(!local.database_require_runtime_role);
        let production = config_with(&[
            ("DATABASE_MIGRATE_ON_START", "false"),
            ("DATABASE_REQUIRE_RUNTIME_ROLE", "true"),
        ])
        .unwrap();
        assert!(!production.database_migrate_on_start);
        assert!(production.database_require_runtime_role);
        assert_eq!(
            config_with(&[("DATABASE_REQUIRE_RUNTIME_ROLE", "true")])
                .err()
                .unwrap()
                .key,
            "DATABASE_MIGRATE_ON_START"
        );
        assert!(config_with(&[("DATABASE_MIGRATE_ON_START", "yes")]).is_err());
        assert!(config_with(&[("DATABASE_REQUIRE_RUNTIME_ROLE", "1")]).is_err());
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
    fn realtime_settings_accept_explicit_origin_limits_and_timeouts() {
        let config = config_with(&[
            (
                "ALLOWED_ORIGINS",
                "https://canvas.example.com, http://localhost:5173",
            ),
            ("WS_MAX_CONNECTIONS", "30"),
            ("WS_MAX_MESSAGE_BYTES", "65536"),
            ("SNAPSHOT_PAGE_SIZE", "50"),
            ("SNAPSHOT_CHUNK_BYTES", "131072"),
            ("WS_HELLO_TIMEOUT_MS", "2500"),
            ("WS_CREATES_PER_SECOND", "3"),
            ("VIEWPORT_MAX_SPAN", "12000"),
        ])
        .unwrap();
        assert_eq!(
            config.realtime.allowed_origins,
            ["https://canvas.example.com", "http://localhost:5173"]
        );
        assert_eq!(config.realtime.max_connections, 30);
        assert_eq!(config.realtime.max_message_bytes, 65536);
        assert_eq!(config.realtime.snapshot_page_size, 50);
        assert_eq!(config.realtime.snapshot_chunk_bytes, 131072);
        assert_eq!(config.realtime.hello_timeout, Duration::from_millis(2500));
        assert_eq!(config.realtime.creates_per_second, 3);
        assert_eq!(config.realtime.viewport_max_span, 12000.0);
    }

    #[test]
    fn rejects_unsafe_realtime_settings() {
        for (key, value) in [
            ("ALLOWED_ORIGINS", ""),
            ("ALLOWED_ORIGINS", "*"),
            ("ALLOWED_ORIGINS", "https://canvas.example.com/"),
            (
                "ALLOWED_ORIGINS",
                "https://user:password@canvas.example.com",
            ),
            ("ALLOWED_ORIGINS", "ftp://canvas.example.com"),
            ("WS_MAX_CONNECTIONS", "0"),
            ("WS_MAX_MESSAGE_BYTES", "1048577"),
            ("SNAPSHOT_CHUNK_BYTES", "100"),
            ("SNAPSHOT_PAGE_SIZE", "1001"),
            ("MUTATION_QUEUE_CAPACITY", "0"),
            ("EVENT_BUFFER_CAPACITY", "0"),
            ("WS_HELLO_TIMEOUT_MS", "3600001"),
            ("WS_SEND_TIMEOUT_MS", "0"),
            ("WS_IDLE_TIMEOUT_MS", "10000"),
            ("VIEWPORT_MAX_SPAN", "NaN"),
            ("VIEWPORT_MAX_SPAN", "0"),
        ] {
            assert_eq!(config_with(&[(key, value)]).err().unwrap().key, key);
        }
    }

    #[test]
    fn passkey_admin_is_disabled_by_default_and_needs_only_https_rp_configuration() {
        assert!(config_with(&[]).unwrap().admin.is_none());
        let settings = [
            ("ADMIN_ENABLED", "true"),
            ("ADMIN_ORIGIN", "https://canvas.example.test"),
            ("ADMIN_RP_ID", "example.test"),
        ];
        let config = config_with(&settings).unwrap().admin.unwrap();
        assert_eq!(config.origin.as_str(), "https://canvas.example.test/");
        assert_eq!(config.rp_id, "example.test");
        // No CA file, authenticator certificate or device-specific secret is required.
        for (key, value) in [
            ("ADMIN_ORIGIN", "http://canvas.example.test"),
            ("ADMIN_ORIGIN", "https://canvas.example.test/path"),
            ("ADMIN_RP_ID", "unrelated.test"),
        ] {
            let mut invalid = settings.to_vec();
            invalid.iter_mut().find(|(name, _)| *name == key).unwrap().1 = value;
            assert_eq!(config_with(&invalid).err().unwrap().key, "ADMIN_CONFIG");
        }
        assert_eq!(
            config_with(&[("ADMIN_ENABLED", "true")]).err().unwrap().key,
            "ADMIN_ORIGIN"
        );
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
