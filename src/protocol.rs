//! JSON v1 messages. No client message or internal owner record is Debug-logged.

use serde::{Deserialize, Serialize, Serializer, ser::SerializeMap};
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
        #[serde(default, deserialize_with = "optional_viewport")]
        visible_bounds: Option<ViewportInput>,
    },
    #[serde(rename = "viewport.subscribe.cached")]
    CachedSubscribe {
        subscription_id: String,
        bounds: ViewportInput,
        #[serde(default, deserialize_with = "optional_viewport")]
        visible_bounds: Option<ViewportInput>,
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
    /// v5 only (docs/protocol-v5-safety.md).
    #[serde(rename = "stroke.report")]
    Report {
        request_id: String,
        stroke_ids: Vec<Uuid>,
        reason: ReportReason,
        block_author: bool,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ReportReason {
    Sexual,
    Violence,
    Hate,
    Illegal,
    Spam,
    Other,
}

impl ReportReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Sexual => "sexual",
            Self::Violence => "violence",
            Self::Hate => "hate",
            Self::Illegal => "illegal",
            Self::Spam => "spam",
            Self::Other => "other",
        }
    }
}

/// At most this many strokes per report (docs/protocol-v5-safety.md).
pub(crate) const MAX_REPORT_STROKES: usize = 20;

fn optional_viewport<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<ViewportInput>, D::Error> {
    ViewportInput::deserialize(deserializer).map(Some)
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

#[derive(Clone)]
pub(crate) struct PublicStroke {
    pub input: StrokeInput,
    pub seq: i64,
    pub own: bool,
    /// v5+: per-viewer author tag (docs/protocol-v5-safety.md).
    pub author: Option<String>,
    /// v6: geometry goes out as `path` instead of `points` (docs/protocol-v6-compact.md).
    pub compact: bool,
}

/// v6 path quantum: coordinates are integers in 1/16 world unit, a quarter of a
/// CSS pixel at the clients' maximum zoom (4).
pub(crate) const PATH_SCALE: f64 = 16.0;

/// `[x0, y0, dx1, dy1, …]`: the first point absolute, then deltas, all in 1/16 world
/// unit. Deltas are taken between rounded points, so decoding has no drift. Every
/// value is within ±(WORLD_LIMIT·16·2), far inside the protocol's safe-integer range.
pub(crate) fn path(points: &[[f64; 2]]) -> Vec<i64> {
    let mut path = Vec::with_capacity(points.len() * 2);
    let (mut previous_x, mut previous_y) = (0, 0);
    for &[x, y] in points {
        // Validated points are finite and within WORLD_LIMIT, so the casts are exact.
        let (x, y) = (
            (x * PATH_SCALE).round() as i64,
            (y * PATH_SCALE).round() as i64,
        );
        path.push(x - previous_x);
        path.push(y - previous_y);
        (previous_x, previous_y) = (x, y);
    }
    path
}

impl Serialize for PublicStroke {
    // Field order and names match the derived v1–v5 object; only `points` becomes `path` in v6.
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let input = &self.input;
        let mut map = serializer.serialize_map(Some(8 + usize::from(self.author.is_some())))?;
        map.serialize_entry("id", &input.id)?;
        if self.compact {
            map.serialize_entry("path", &path(&input.points))?;
        } else {
            map.serialize_entry("points", &input.points)?;
        }
        map.serialize_entry("brush", &input.brush)?;
        map.serialize_entry("width", &input.width)?;
        map.serialize_entry("color", &input.color)?;
        map.serialize_entry("opacity", &input.opacity)?;
        map.serialize_entry("seq", &self.seq)?;
        map.serialize_entry("own", &self.own)?;
        if let Some(author) = &self.author {
            map.serialize_entry("author", author)?;
        }
        map.end()
    }
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
    #[serde(rename = "stroke.report")]
    Report { report_id: Uuid },
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
    /// v5: this session was banned by a moderator; creates and reports are refused.
    Banned,
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
            Self::Banned => "this session can no longer draw or report",
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

    fn public(compact: bool, points: Vec<[f64; 2]>) -> PublicStroke {
        PublicStroke {
            input: StrokeInput {
                id: Uuid::nil(),
                points,
                brush: crate::domain::Brush::Round,
                width: 4.0,
                color: "#123456".into(),
                opacity: 1.0,
            },
            seq: 7,
            own: false,
            author: None,
            compact,
        }
    }

    #[test]
    fn path_is_absolute_then_deltas_in_sixteenths_without_drift() {
        let points = vec![[10.0, -2.5], [10.03, -2.47], [10.06, -2.44], [-1e9, 1e9]];
        let encoded = path(&points);
        assert_eq!(&encoded[..6], &[160, -40, 0, 0, 1, 1]);
        let (mut x, mut y) = (0i64, 0i64);
        for (pair, original) in encoded.chunks(2).zip(&points) {
            (x, y) = (x + pair[0], y + pair[1]);
            let decoded = [x as f64 / PATH_SCALE, y as f64 / PATH_SCALE];
            assert!((decoded[0] - original[0]).abs() <= 0.5 / PATH_SCALE);
            assert!((decoded[1] - original[1]).abs() <= 0.5 / PATH_SCALE);
        }
        assert_eq!((x, y), (-16_000_000_000, 16_000_000_000));
        assert!(
            encoded
                .iter()
                .all(|value| value.unsigned_abs() <= 9_007_199_254_740_991)
        );
    }

    #[test]
    fn v6_strokes_replace_points_with_path_and_keep_every_other_field() {
        let points = vec![[0.0, 0.0], [1.0, 0.5]];
        let full = serde_json::to_value(public(false, points.clone())).unwrap();
        let compact = serde_json::to_value(public(true, points)).unwrap();
        assert_eq!(full["points"], serde_json::json!([[0.0, 0.0], [1.0, 0.5]]));
        assert_eq!(compact["path"], serde_json::json!([0, 0, 16, 8]));
        assert!(compact.get("points").is_none() && full.get("path").is_none());
        let (mut full, mut compact) = (full, compact);
        full.as_object_mut().unwrap().remove("points");
        compact.as_object_mut().unwrap().remove("path");
        assert_eq!(full, compact);
        assert_eq!(
            full,
            serde_json::json!({"id": Uuid::nil(), "brush": "round", "width": 4.0,
                "color": "#123456", "opacity": 1.0, "seq": 7, "own": false})
        );
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
