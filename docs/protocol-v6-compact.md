# WebSocket v6: integer stroke paths

20.11.2026. Normative extension of [v5](protocol-v5-safety.md). Everything in v5 (negotiation shape, subscriptions, manifest/fetch, visible-first order, author tags, reports, limits) is unchanged unless stated here. This document is not a deployment claim.

## Why

A maximum-zoom-out view of production (4784 strokes, 522 000 points) was a 10.7 MB snapshot in v5: every point is two MessagePack float64, about 20 bytes. The same snapshot in v6 is 2.24 MB (−79%); a typical point costs 2–4 bytes. Measured on 20.11.2026 with a read-only subscription; no other part of the protocol changes.

## Negotiation

Preferred subprotocol: `endless-drawing.msgpack.v6`; `hello.ack.protocol_version` is 6. Clients offer v6, v5, v4, v3, v2, explicit JSON v1 in that order. Selection requires all v5 gates plus `WS_COMPACT_ENABLED` (default true); disabling it selects v5 for new connections. A socket never changes format in place. v5 and older clients are served exactly as before, from the same stored strokes.

## Public stroke objects

Every server-sent stroke in v6 — snapshot and fetch chunks, `stroke.created`, the `stroke.create` ack — carries `path` **instead of** `points`. All other fields, their order and meaning are those of v5 (`id`, `brush`, `width`, `color`, `opacity`, `seq`, `own`, `author`). A v6 stroke object with `points`, or without `path`, is invalid.

```json
{"id":"…","path":[160,-40,1,1,127,-32],"brush":"round","width":4.5,"color":"#E46C60","opacity":0.75,"seq":42,"own":false,"author":"…"}
```

`path` is a flat array of integers `[x0, y0, dx1, dy1, …, dxN, dyN]` in units of **1/16 world unit**:

- the first pair is the absolute first point, every next pair is the delta to the previous point;
- the server rounds each stored point to the nearest 1/16 (`round(v · 16)`, ties away from zero) and takes deltas between the **rounded** points, so decoding accumulates no error: `xᵢ = (x0 + dx1 + … + dxᵢ) / 16`;
- the length is even, 2…4000 (1…2000 points, the existing stroke limit);
- every value is a MessagePack integer (positive/negative fixint, int8…int64, uint8…uint64) within ±9 007 199 254 740 991. Floats, `nil` and other types are invalid. Absolute values are at most 1.6·10¹⁰ (WORLD_LIMIT·16) and deltas at most 1.6·10⁶ (stroke span·16), so JavaScript numbers decode them exactly.

The rounding error is at most 1/32 world unit — 1/8 CSS pixel at the clients' maximum zoom of 4. Clients render the decoded points exactly like v5 `points`. Geometry is not simplified; level of detail is a separate future change.

Stored geometry keeps full precision: v5 and older viewers keep receiving the original floating-point `points`, and the database is unchanged by v6.

## Client messages

Unchanged. `stroke.create` still sends floating-point `points` (uplink is small and the server needs no second validation path). Consequently the `stroke.create` ack carries the rounded `path`, which may differ from the sent points by up to 1/32 world unit; clients must not require them to be equal. Replacing the local geometry with the acknowledged path is allowed but not required.

## Limits

- Server arrays in v6 may hold up to 4000 elements (a 2000-point `path`); client arrays keep the 2000-element limit. Map, depth, string and integer limits are those of v2.
- The per-chunk point budget (8192) and the 1000-stroke and 64 KiB chunk targets are unchanged; chunks simply carry more strokes.
- `viewport.manifest.begin.point_count` keeps counting points, not `path` elements. Since migration 0006 the server reads it from a stored column instead of the JSON geometry.

## Fixtures

`tests/fixtures/ws-v6`: synthetic v6 server messages from the Rust and TypeScript encoders, including int8/int16 boundaries and ±1.6·10¹⁰ coordinates. Clients must decode both encoders' bytes to the same values.
