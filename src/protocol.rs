//! JSON v1 messages. No client message or internal owner record is Debug-logged.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::domain::{Bounds, StrokeInput, WORLD_LIMIT};

#[derive(Deserialize)]
#[serde(tag = "type", deny_unknown_fields)]
pub(crate) enum ClientMessage {
    #[serde(rename = "hello")]
    Hello { session_secret: String },
    #[serde(rename = "viewport.subscribe")]
    Subscribe {
        subscription_id: String,
        bounds: ViewportInput,
    },
    #[serde(rename = "viewport.subscribe.cached")]
    CachedSubscribe {
        subscription_id: String,
        bounds: ViewportInput,
    },
    #[serde(rename = "viewport.fetch")]
    Fetch {
        subscription_id: String,
        snapshot_token: Uuid,
        batch: u32,
        stroke_ids: Vec<Uuid>,
        #[serde(rename = "final")]
        final_batch: bool,
    },
    #[serde(rename = "stroke.create")]
    Create {
        request_id: String,
        stroke: StrokeInput,
    },
    #[serde(rename = "stroke.delete")]
    Delete { request_id: String, stroke_id: Uuid },
}

#[derive(Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ViewportInput {
    pub min_x: f64,
    pub min_y: f64,
    pub max_x: f64,
    pub max_y: f64,
}

impl ViewportInput {
    pub fn validate(self, max_span: f64) -> Result<Bounds, ErrorCode> {
        let coordinates = [self.min_x, self.min_y, self.max_x, self.max_y];
        if coordinates
            .iter()
            .any(|value| !value.is_finite() || value.abs() > WORLD_LIMIT)
            || self.min_x >= self.max_x
            || self.min_y >= self.max_y
            || self.max_x - self.min_x > max_span
            || self.max_y - self.min_y > max_span
        {
            return Err(ErrorCode::ValidationFailed);
        }
        Ok(Bounds {
            min_x: self.min_x,
            min_y: self.min_y,
            max_x: self.max_x,
            max_y: self.max_y,
        })
    }
}

#[derive(Clone, Serialize)]
pub(crate) struct PublicStroke {
    #[serde(flatten)]
    pub input: StrokeInput,
    pub seq: i64,
    pub own: bool,
}

#[derive(Serialize)]
#[serde(tag = "operation")]
pub(crate) enum AckResult {
    #[serde(rename = "stroke.create")]
    Create { stroke: PublicStroke },
    #[serde(rename = "stroke.delete")]
    Delete {
        stroke_id: Uuid,
        seq: i64,
        already_deleted: bool,
    },
}

#[derive(Serialize, Debug, Clone, Copy, PartialEq)]
pub(crate) struct SpawnPoint {
    pub x: f64,
    pub y: f64,
}

#[derive(Serialize)]
#[serde(tag = "type")]
pub(crate) enum ServerMessage {
    #[serde(rename = "hello.ack")]
    HelloAck {
        protocol_version: u8,
        /// Optional entry hint for a new session (see `spawn.rs`); absent for an empty world.
        #[serde(skip_serializing_if = "Option::is_none")]
        spawn: Option<SpawnPoint>,
    },
    #[serde(rename = "viewport.snapshot.begin")]
    SnapshotBegin { subscription_id: String },
    #[serde(rename = "viewport.snapshot.chunk")]
    SnapshotChunk {
        subscription_id: String,
        strokes: Vec<PublicStroke>,
    },
    #[serde(rename = "viewport.snapshot.end")]
    SnapshotEnd { subscription_id: String },
    #[serde(rename = "viewport.manifest.begin")]
    ManifestBegin {
        subscription_id: String,
        snapshot_token: Uuid,
        stroke_count: usize,
        point_count: usize,
    },
    #[serde(rename = "viewport.manifest.chunk")]
    ManifestChunk {
        subscription_id: String,
        snapshot_token: Uuid,
        stroke_ids: Vec<Uuid>,
    },
    #[serde(rename = "viewport.manifest.end")]
    ManifestEnd {
        subscription_id: String,
        snapshot_token: Uuid,
    },
    #[serde(rename = "viewport.fetch.end")]
    FetchEnd {
        subscription_id: String,
        snapshot_token: Uuid,
        batch: u32,
    },
    #[serde(rename = "viewport.snapshot.fallback")]
    SnapshotFallback {
        subscription_id: String,
        reason: &'static str,
    },
    #[serde(rename = "stroke.created")]
    Created {
        subscription_id: String,
        stroke: PublicStroke,
    },
    #[serde(rename = "stroke.deleted")]
    Deleted {
        subscription_id: String,
        stroke_id: Uuid,
        seq: i64,
    },
    #[serde(rename = "ack")]
    Ack {
        request_id: String,
        #[serde(flatten)]
        result: AckResult,
    },
    #[serde(rename = "error")]
    Error {
        #[serde(skip_serializing_if = "Option::is_none")]
        request_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        subscription_id: Option<String>,
        code: ErrorCode,
        message: &'static str,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ErrorCode {
    InvalidMessage,
    InvalidSession,
    HelloRequired,
    ValidationFailed,
    Forbidden,
    NotFound,
    IdConflict,
    StrokeDeleted,
    RateLimited,
    ResyncRequired,
    InternalError,
}

impl ErrorCode {
    pub fn message(self) -> &'static str {
        match self {
            Self::InvalidMessage => "invalid message",
            Self::InvalidSession => "invalid session secret",
            Self::HelloRequired => "hello must be completed first",
            Self::ValidationFailed => "invalid stroke or viewport",
            Self::Forbidden => "stroke cannot be deleted",
            Self::NotFound => "stroke not found",
            Self::IdConflict => "stroke id is already in use",
            Self::StrokeDeleted => "stroke has already been deleted",
            Self::RateLimited => "operation limit exceeded; retry later",
            Self::ResyncRequired => "subscribe again to obtain a fresh snapshot",
            Self::InternalError => "operation could not be completed",
        }
    }

    pub fn response(
        self,
        request_id: Option<String>,
        subscription_id: Option<String>,
    ) -> ServerMessage {
        ServerMessage::Error {
            request_id,
            subscription_id,
            code: self,
            message: self.message(),
        }
    }
}

pub(crate) fn valid_id(value: &str) -> bool {
    !value.is_empty() && value.len() <= 128
}

pub(crate) fn intersects(first: Bounds, second: Bounds) -> bool {
    first.min_x <= second.max_x
        && first.max_x >= second.min_x
        && first.min_y <= second.max_y
        && first.max_y >= second.min_y
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn viewport_must_be_finite_ordered_and_bounded() {
        let area = ViewportInput {
            min_x: -10.0,
            min_y: -10.0,
            max_x: 10.0,
            max_y: 10.0,
        };
        assert!(area.validate(20.0).is_ok());
        for invalid in [
            ViewportInput {
                max_x: f64::NAN,
                ..area
            },
            ViewportInput {
                min_y: f64::NEG_INFINITY,
                ..area
            },
            ViewportInput {
                max_x: WORLD_LIMIT + 1.0,
                ..area
            },
            ViewportInput {
                max_y: -10.0,
                ..area
            },
            ViewportInput {
                min_x: 11.0,
                ..area
            },
            ViewportInput {
                max_x: 11.0,
                ..area
            },
        ] {
            assert_eq!(invalid.validate(20.0), Err(ErrorCode::ValidationFailed));
        }
    }

    #[test]
    fn client_fields_are_strict_and_errors_do_not_contain_payloads() {
        assert!(
            serde_json::from_str::<ClientMessage>(
                r#"{"type":"hello","session_secret":"x","own":true}"#
            )
            .is_err()
        );
        let error = ErrorCode::Forbidden.response(Some("r2".into()), None);
        assert_eq!(
            serde_json::to_value(error).unwrap(),
            serde_json::json!({
                "type":"error", "request_id":"r2", "code":"forbidden", "message":"stroke cannot be deleted"
            })
        );
        assert!(!valid_id(""));
        assert!(!valid_id(&"x".repeat(129)));
        assert!(valid_id("r1"));
    }
}
