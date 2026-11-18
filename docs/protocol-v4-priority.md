# WebSocket v4: visible geometry before prefetch

M3, 18.11.2026. This is the normative extension of [manifest v3](protocol-v3-manifest.md). Mutations, ownership, sequence composition, brush geometry, limits and snapshot consistency are unchanged. Implementation and acceptance are recorded separately; this document is not a deployment claim.

## Negotiation

Preferred subprotocol: `endless-drawing.msgpack.v4`; hello.ack.protocol_version is 4. The client also offers v3/v2/explicit JSON v1. Selection requires WS_MSGPACK_ENABLED, WS_MANIFEST_ENABLED and WS_PRIORITY_ENABLED (default true). Disabling the last gate selects v3 for new connections. A socket never changes format in place. v1/v2/v3 subscription forms remain unchanged.

## Subscription hint

Both `viewport.subscribe` and `viewport.subscribe.cached` require `visible_bounds` in v4, with the same finite ordered rectangle fields as `bounds`: min_x/min_y/max_x/max_y. It must be nonempty, inside subscription bounds and world limits, with each span <= VIEWPORT_MAX_SPAN. A missing/malformed hint is invalid_message (MessagePack null fails structural validation and closes with 1007 before application parsing); an invalid rectangle or one outside bounds is validation_failed. Other profiles reject the hint as invalid_message. subscription_id/rate limits stay unchanged.

The client intersects the visible window with subscription/world bounds before sending. A very wide window at 16% cannot prioritize world geometry outside the maximum 30000-unit subscription. The hint is captured for each subscription; it does not expand membership or restart a snapshot while the camera moves inside the existing prefetch margin.

Example v4 subscription (MessagePack map; JSON shown for readability):

```json
{"type":"viewport.subscribe","subscription_id":"area-1","bounds":{"min_x":-1500,"min_y":-1050,"max_x":1500,"max_y":1050},"visible_bounds":{"min_x":-500,"min_y":-350,"max_x":500,"max_y":350}}
```

## Full snapshot

One read snapshot and one begin/chunks/end barrier cover the complete subscription. Strokes with conservative stored bounds intersecting visible_bounds are sent first in ascending seq; other members follow in ascending seq. A member occurs once, including strokes touching or spanning both areas. The classification uses inclusive bounds intersection, not pixel intersection. An empty visible class starts prefetch immediately.

The two passes use the same transaction and keyset pagination; they do not materialize an unbounded geometry set or add a manifest/fetch round trip on cold/reconnect/resync. The transaction is committed before snapshot.end, as before. Existing timeout/backpressure/page limits remain effective.

## Cached snapshot

The consistent metadata scan emits the manifest ids in the same visible-first, then prefetch order (seq within each class). It counts the same membership/points as v3 and commits before waiting for fetch. No geometry is read for known ids. Client walks this order, retains cached immutable objects and requests only missing ids in the existing bounded stop-and-wait batches. The server preserves requested id order within each batch; a client deliberately requesting offscreen ids first forfeits the performance hint, not authorization or snapshot integrity.

Token, scope, duplicate id, batch validation and busy/scene_limit fallbacks are identical to v3. Fetch reads immutable snapshot-member rows, including later soft-deleted rows; physical purge/seq mismatch requires resync. Delete/hide/create events after the snapshot boundary are delivered after end. Authoritative removal of absent ids still occurs only on end. Reconnect/resync always start full snapshots.

## Composition and limits

Arrival order is not drawing order. Clients always compose confirmed/staging strokes by seq and invalidate affected tiles when an older stroke arrives. Global monotonic seq across chunks is not promised in v4. No new server message types or progress marker are introduced. In particular, first chunk, manifest.end and snapshot.end do not prove that the browser has painted.

The v2 float64/UUID/MessagePack byte and structural limits, v3 20k/1M admission bounds, bounded queues, connection pool, snapshot/fetch timeout and existing rate limits remain unchanged. No database migration or persistent geometry cache is required.

## Acceptance

Pinned Rust/TS fixtures cover full/cached subscription and hello ack. Real PostgreSQL/WS tests cover priority before old offscreen rows, each member exactly once, partition boundary, empty visible area, malformed hints, v3 fallback and warm missing-only order. Existing deletion/cancellation/overflow/own races remain regression gates. Browser comparison uses a fixed synthetic corpus and shaped local WS link; report first visible geometry, verified canvas paint, full end, bytes and scene equality separately, including p50/p95 and sample count. Physical iPhone/iPad remains a separate acceptance gate.
