//! Connection-scoped wire profiles. MessagePack is bounded before serde allocation.

use std::collections::HashSet;

use axum::extract::ws::Message;
use serde::Serialize;
use serde_json::Value;

pub(crate) const SAFETY_PROTOCOL: &str = "endless-drawing.msgpack.v5";
pub(crate) const PRIORITY_PROTOCOL: &str = "endless-drawing.msgpack.v4";
pub(crate) const MANIFEST_PROTOCOL: &str = "endless-drawing.msgpack.v3";
pub(crate) const MSGPACK_PROTOCOL: &str = "endless-drawing.msgpack.v2";
pub(crate) const JSON_PROTOCOL: &str = "endless-drawing.json.v1";
pub(crate) const CLIENT_BYTES: usize = 128 * 1024;
pub(crate) const SERVER_BYTES: usize = 256 * 1024;
const SAFE_INTEGER: u64 = 9_007_199_254_740_991;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Profile {
    Json,
    MessagePack,
    Manifest,
    Priority,
    /// v4 plus author tags and `stroke.report` (docs/protocol-v5-safety.md).
    Safety,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DecodeError {
    Invalid,
    Oversize,
    WrongType,
}

impl DecodeError {
    pub fn close_code(self) -> u16 {
        match self {
            Self::Invalid => 1007,
            Self::Oversize => 1009,
            Self::WrongType => 1003,
        }
    }
}

impl Profile {
    pub fn binary(self) -> bool {
        self != Self::Json
    }

    /// v3+: cached subscriptions, manifest and fetch.
    pub fn manifest(self) -> bool {
        matches!(self, Self::Manifest | Self::Priority | Self::Safety)
    }

    /// v4+: `visible_bounds` is required in subscriptions.
    pub fn visible_first(self) -> bool {
        matches!(self, Self::Priority | Self::Safety)
    }

    /// v5: author tags on strokes and `stroke.report`.
    pub fn safety(self) -> bool {
        self == Self::Safety
    }

    pub fn version(self) -> u8 {
        match self {
            Self::Json => 1,
            Self::MessagePack => 2,
            Self::Manifest => 3,
            Self::Priority => 4,
            Self::Safety => 5,
        }
    }

    pub fn encode<T: Serialize>(self, value: &T) -> Result<Vec<u8>, ()> {
        if self == Self::Json {
            return serde_json::to_vec(value).map_err(|_| ());
        }
        let mut bytes = Vec::new();
        // UUIDs must remain strings even in a binary serializer. Maps retain names.
        value
            .serialize(
                &mut rmp_serde::Serializer::new(&mut bytes)
                    .with_struct_map()
                    .with_human_readable(),
            )
            .map_err(|_| ())?;
        validate(&bytes, SERVER_BYTES).map_err(|_| ())?;
        Ok(bytes)
    }

    pub fn frame<T: Serialize>(self, value: &T) -> Result<Message, ()> {
        let bytes = self.encode(value)?;
        Ok(match self {
            Self::Json => Message::Text(String::from_utf8(bytes).map_err(|_| ())?.into()),
            Self::MessagePack | Self::Manifest | Self::Priority | Self::Safety => {
                Message::Binary(bytes.into())
            }
        })
    }

    pub fn decode(self, frame: Message) -> Result<Value, DecodeError> {
        match (self, frame) {
            (Self::Json, Message::Text(text)) => {
                serde_json::from_str(&text).map_err(|_| DecodeError::Invalid)
            }
            (
                Self::MessagePack | Self::Manifest | Self::Priority | Self::Safety,
                Message::Binary(bytes),
            ) => {
                validate(&bytes, CLIENT_BYTES)?;
                rmp_serde::from_slice(&bytes).map_err(|_| DecodeError::Invalid)
            }
            _ => Err(DecodeError::WrongType),
        }
    }
}

/// Check all headers, nesting, keys and int64 range before a general decoder.
fn validate(bytes: &[u8], limit: usize) -> Result<(), DecodeError> {
    if bytes.len() > limit {
        return Err(DecodeError::Oversize);
    }
    if !bytes
        .first()
        .is_some_and(|tag| matches!(tag, 0x80..=0x8f | 0xde | 0xdf))
    {
        return Err(DecodeError::Invalid);
    }
    let mut scanner = Scanner {
        bytes,
        offset: 0,
        nodes: 0,
    };
    scanner.read(0, false)?;
    if scanner.offset != bytes.len() {
        return Err(DecodeError::Invalid);
    }
    Ok(())
}

struct Scanner<'a> {
    bytes: &'a [u8],
    offset: usize,
    nodes: usize,
}

impl<'a> Scanner<'a> {
    fn take(&mut self, length: usize) -> Result<&'a [u8], DecodeError> {
        let end = self
            .offset
            .checked_add(length)
            .ok_or(DecodeError::Invalid)?;
        let result = self
            .bytes
            .get(self.offset..end)
            .ok_or(DecodeError::Invalid)?;
        self.offset = end;
        Ok(result)
    }

    fn number<const N: usize>(&mut self) -> Result<[u8; N], DecodeError> {
        self.take(N)?.try_into().map_err(|_| DecodeError::Invalid)
    }

    fn length(&mut self, size: usize) -> Result<usize, DecodeError> {
        Ok(match size {
            1 => usize::from(self.number::<1>()?[0]),
            2 => usize::from(u16::from_be_bytes(self.number()?)),
            _ => usize::try_from(u32::from_be_bytes(self.number()?))
                .map_err(|_| DecodeError::Invalid)?,
        })
    }

    fn read(&mut self, depth: usize, key: bool) -> Result<Option<&'a str>, DecodeError> {
        self.nodes += 1;
        if self.nodes > 65_536 {
            return Err(DecodeError::Invalid);
        }
        let tag = self.number::<1>()?[0];
        if matches!(tag, 0xa0..=0xbf | 0xd9..=0xdb) {
            let length = match tag {
                0xa0..=0xbf => usize::from(tag & 31),
                0xd9 => self.length(1)?,
                0xda => self.length(2)?,
                _ => self.length(4)?,
            };
            if length > 256 {
                return Err(DecodeError::Invalid);
            }
            return std::str::from_utf8(self.take(length)?)
                .map(Some)
                .map_err(|_| DecodeError::Invalid);
        }
        if key {
            return Err(DecodeError::Invalid);
        }
        match tag {
            0x00..=0x7f | 0xe0..=0xff | 0xc2 | 0xc3 => return Ok(None),
            0xcc | 0xd0 => {
                self.take(1)?;
                return Ok(None);
            }
            0xcd | 0xd1 => {
                self.take(2)?;
                return Ok(None);
            }
            0xce | 0xd2 => {
                self.take(4)?;
                return Ok(None);
            }
            0xcf => {
                if u64::from_be_bytes(self.number()?) > SAFE_INTEGER {
                    return Err(DecodeError::Invalid);
                }
                return Ok(None);
            }
            0xd3 => {
                if i64::from_be_bytes(self.number()?).unsigned_abs() > SAFE_INTEGER {
                    return Err(DecodeError::Invalid);
                }
                return Ok(None);
            }
            0xcb => {
                if !f64::from_be_bytes(self.number()?).is_finite() {
                    return Err(DecodeError::Invalid);
                }
                return Ok(None);
            }
            _ => {}
        }
        let map = matches!(tag, 0x80..=0x8f | 0xde | 0xdf);
        let array = matches!(tag, 0x90..=0x9f | 0xdc | 0xdd);
        if (!map && !array) || depth >= 8 {
            return Err(DecodeError::Invalid);
        }
        let count = match tag {
            0x80..=0x9f => usize::from(tag & 15),
            0xdc | 0xde => self.length(2)?,
            _ => self.length(4)?,
        };
        if count > if map { 16 } else { 2000 }
            || count * if map { 2 } else { 1 } > self.bytes.len() - self.offset
        {
            return Err(DecodeError::Invalid);
        }
        let mut keys = HashSet::new();
        for _ in 0..count {
            if map {
                let name = self.read(depth + 1, true)?.ok_or(DecodeError::Invalid)?;
                if matches!(name, "__proto__" | "prototype" | "constructor") || !keys.insert(name) {
                    return Err(DecodeError::Invalid);
                }
            }
            self.read(depth + 1, false)?;
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::ServerMessage;
    use uuid::Uuid;

    fn directory() -> std::path::PathBuf {
        let version = std::env::var("WS_FIXTURES_VERSION").unwrap_or_else(|_| "ws-v2".into());
        assert!(["ws-v2", "ws-v3", "ws-v4"].contains(&version.as_str()));
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(version)
    }

    #[test]
    fn reads_typescript_and_rust_fixture_corpus() {
        for version in ["ws-v2", "ws-v3", "ws-v4"] {
            let directory = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures")
                .join(version);
            use sha2::{Digest, Sha256};
            let hashes: std::collections::BTreeMap<String, String> =
                serde_json::from_slice(&std::fs::read(directory.join("sha256.json")).unwrap())
                    .unwrap();
            for (name, hash) in hashes {
                assert_eq!(
                    format!(
                        "{:x}",
                        Sha256::digest(std::fs::read(directory.join(name)).unwrap())
                    ),
                    hash
                );
            }
            let corpus: Vec<Value> =
                serde_json::from_slice(&std::fs::read(directory.join("corpus.json")).unwrap())
                    .unwrap();
            for case in corpus {
                let name = case["name"].as_str().unwrap();
                for encoder in ["ts", "rust"] {
                    let bytes =
                        std::fs::read(directory.join(format!("{name}.{encoder}.msgpack"))).unwrap();
                    let actual = Profile::MessagePack
                        .decode(Message::Binary(bytes.into()))
                        .unwrap();
                    assert_eq!(actual, case["value"], "{name}/{encoder}");
                }
            }
        }
    }

    #[test]
    fn typed_server_uuid_is_a_string_and_floats_are_full_precision() {
        let id = Uuid::new_v4();
        let message = ServerMessage::Deleted {
            subscription_id: "view".into(),
            stroke_id: id,
            seq: 9_007_199_254_740_991,
        };
        let frame = Profile::MessagePack.frame(&message).unwrap();
        let value = Profile::MessagePack.decode(frame).unwrap();
        assert_eq!(value["stroke_id"], id.to_string());
        let invalid = ServerMessage::Deleted {
            subscription_id: "view".into(),
            stroke_id: id,
            seq: 9_007_199_254_740_992,
        };
        assert!(Profile::MessagePack.frame(&invalid).is_err());
    }

    #[test]
    fn rejects_hostile_headers_and_non_profile_types_before_serde() {
        let inputs: &[&[u8]] = &[
            &[],
            &[0xc0],
            &[0x90],
            &[0xdf, 255, 255, 255, 255],
            &[0x81, 0xa1, b'a', 0xdd, 255, 255, 255, 255],
            &[0x82, 0xa1, b'a', 1, 0xa1, b'a', 2],
            &[0x81, 1, 1],
            &[0x81, 0xa1, b'a', 0xcf, 0, 32, 0, 0, 0, 0, 0, 0],
            &[0x81, 0xa1, b'a', 0xca, 0, 0, 0, 0],
            &[0x80, 0x80],
            &[0x81, 0xa1, 255, 1],
            &[0x81, 0xa1, b'a', 0xc4, 0],
        ];
        for bytes in inputs {
            assert_eq!(validate(bytes, CLIENT_BYTES), Err(DecodeError::Invalid));
        }
        assert_eq!(
            validate(&vec![0; CLIENT_BYTES + 1], CLIENT_BYTES),
            Err(DecodeError::Oversize)
        );
        assert_eq!(
            Profile::MessagePack.decode(Message::Text("{}".into())),
            Err(DecodeError::WrongType)
        );
        let bytes = Profile::MessagePack
            .encode(&serde_json::json!({"point": [999_999_999.125, -999_999_999.062_5]}))
            .unwrap();
        for end in 0..bytes.len() {
            assert!(validate(&bytes[..end], CLIENT_BYTES).is_err());
        }
        let mut nested = serde_json::json!(1);
        for _ in 0..9 {
            nested = serde_json::json!([nested]);
        }
        assert!(
            Profile::MessagePack
                .encode(&serde_json::json!({"nested":nested}))
                .is_err()
        );
    }

    #[test]
    #[ignore = "fixture maintenance: explicitly regenerate after changing the corpus"]
    fn export_rust_fixtures() {
        if std::env::var_os("UPDATE_WS_FIXTURES").is_none() {
            return;
        }
        let corpus: Vec<Value> =
            serde_json::from_slice(&std::fs::read(directory().join("corpus.json")).unwrap())
                .unwrap();
        for case in corpus {
            let name = case["name"].as_str().unwrap();
            std::fs::write(
                directory().join(format!("{name}.rust.msgpack")),
                Profile::MessagePack.encode(&case["value"]).unwrap(),
            )
            .unwrap();
        }
    }
}
