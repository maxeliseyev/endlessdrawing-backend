//! Resource bounds for the single-process realtime service.

use std::time::Duration;

/// Configurable WebSocket limits. All capacities/rates/timeouts must be positive.
#[derive(Debug, Clone)]
pub struct RealtimeSettings {
    pub allowed_origins: Vec<String>,
    pub max_connections: usize,
    /// Allow selection of MessagePack v2; JSON v1 remains supported.
    pub msgpack_enabled: bool,
    /// Allow manifest/missing-geometry v3 when binary transport is enabled.
    pub manifest_enabled: bool,
    pub max_message_bytes: usize,
    pub mutation_queue_capacity: usize,
    pub event_buffer_capacity: usize,
    pub snapshot_page_size: u32,
    pub snapshot_chunk_bytes: usize,
    pub viewport_max_span: f64,
    pub creates_per_second: usize,
    pub deletes_per_second: usize,
    pub subscribes_per_second: usize,
    pub messages_per_second: usize,
    pub hello_timeout: Duration,
    pub send_timeout: Duration,
    pub operation_timeout: Duration,
    pub snapshot_timeout: Duration,
    pub heartbeat_interval: Duration,
    pub idle_timeout: Duration,
    /// How often new-session entry hints are re-sampled from live strokes.
    pub spawn_refresh: Duration,
}

impl Default for RealtimeSettings {
    fn default() -> Self {
        Self {
            allowed_origins: [
                "http://localhost:5173",
                "http://127.0.0.1:5173",
                "http://localhost:3000",
                "http://127.0.0.1:3000",
            ]
            .into_iter()
            .map(str::to_owned)
            .collect(),
            max_connections: 256,
            msgpack_enabled: true,
            manifest_enabled: true,
            max_message_bytes: 128 * 1024,
            mutation_queue_capacity: 128,
            event_buffer_capacity: 256,
            snapshot_page_size: 100,
            snapshot_chunk_bytes: 256 * 1024,
            viewport_max_span: 30_000.0,
            creates_per_second: 5,
            deletes_per_second: 20,
            subscribes_per_second: 5,
            messages_per_second: 100,
            hello_timeout: Duration::from_secs(5),
            send_timeout: Duration::from_secs(5),
            operation_timeout: Duration::from_secs(10),
            snapshot_timeout: Duration::from_secs(30),
            heartbeat_interval: Duration::from_secs(20),
            idle_timeout: Duration::from_secs(60),
            spawn_refresh: Duration::from_secs(30),
        }
    }
}
