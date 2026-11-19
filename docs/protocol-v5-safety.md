# WebSocket v5: reports and author tags (App Review 1.2)

19.11.2026. Normative extension of [v4](protocol-v4-priority.md). Everything in v4 (negotiation shape, subscriptions, snapshots, manifest/fetch, mutations, limits) is unchanged unless stated here. Product design and owner decisions: frontend `docs/ugc-safety.md`. This document is not a deployment claim.

## Negotiation

Preferred subprotocol: `endless-drawing.msgpack.v5`; `hello.ack.protocol_version` is 5. Clients offer v5, v4, v3, v2, explicit JSON v1 in that order. Selection requires all v4 gates plus `WS_SAFETY_ENABLED` (default true); disabling it selects v4 for new connections. The new field and message below are invalid in every other profile.

## Author tag

Every public stroke object in v5 (snapshot and fetch chunks, `stroke.created`, `stroke.create` ack) carries `author`: a 22-character unpadded base64url string, the first 16 bytes of

```
SHA-256("endless-drawing.author.v1" ‖ viewer_owner_digest ‖ stroke_owner_digest)
```

- The tag is stable for one viewer secret and one author, so a client can hide everything from an author it blocked, including future strokes.
- Different viewers get unrelated tags for the same author. Owner digests are never public, so a tag can be neither inverted nor compared across viewers without them. A server key would add nothing: it would live next to the digests in the same database.
- The tag is part of the immutable per-viewer stroke object for the connection; it never changes during a connection.

Blocking is client-side only: the server keeps no block list and does not filter snapshots by author.

## `stroke.report`

```json
{"type":"stroke.report","request_id":"…","stroke_ids":["…"],"reason":"hate","block_author":true}
```

- `request_id`: same rules as other mutations. `stroke_ids`: 1–20 unique UUIDs. `reason`: one of `sexual`, `violence`, `hate`, `illegal`, `spam`, `other`. `block_author`: boolean, informational for moderators (the client hides locally either way).
- Every id must be a currently visible stroke not owned by the reporter; otherwise the whole report is rejected: `not_found` (unknown, deleted, hidden or held) or `validation_failed` (own stroke, duplicates, size/enum). Nothing is stored on rejection.
- Success: `{"type":"ack","request_id":"…","operation":"stroke.report","report_id":"…"}`. A repeated `request_id` from the same connection returns the same ack.
- A reporter counts once per stroke: reporting the same stroke again stores the new report but adds no further weight towards holding.
- Rate limit: 10 reports per rolling hour per owner digest (persisted, survives reconnect), checked before validation; excess gets `rate_limited`.

## Holding by reports

When at least `REPORT_HOLD_THRESHOLD` (default 5) distinct reporters have open reports on a stroke, it becomes **held** in the same transaction: it is removed from public view exactly like a moderator hide (`deleted_at` set, public `stroke.deleted`, absent from snapshots and manifests). A moderator then confirms (it stays hidden, the hold becomes a hide) or dismisses (the stroke is restored and published again as `stroke.created` with its original seq).

Restoration note for clients: v3+ clients that keep a tombstone for the id ignore the restored `stroke.created` until their next connection; a dismissed hold therefore reappears for them after reconnect. This is intentional (tombstones protect against resurrection of real deletes).

## Bans

A moderator ban targets the owner digest of a chosen stroke. In one transaction every visible stroke of that owner is hidden (public `stroke.deleted` each, audited like hide) and the digest is recorded. A banned digest receives `banned` (new error code) for `stroke.create` and `stroke.report`; reads are unaffected. A new anonymous secret evades a ban; repeated abuse is handled by reports and holding.

## Moderator notifications

The application never holds the Telegram token. It exposes loopback-only `GET /internal/moderation/summary` (rejected when a forwarding header is present; Caddy also refuses `/internal/*`), returning only counts:

```json
{"open_reports":3,"held":1,"latest_report_seq":118,"oldest_open_age_s":540}
```

The VPS monitor polls it every minute and sends one Telegram message when `latest_report_seq` grows (reason counts and a link to `/admin/`, no geometry, ids or digests).

## Admin API

Defined in [admin-api.md](admin-api.md) §5 with the existing session, MFA, CSRF, rate limit and audit rules.
