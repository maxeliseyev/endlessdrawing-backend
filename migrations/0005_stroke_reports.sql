-- v5 reports, report-threshold holds and author bans (docs/protocol-v5-safety.md).
CREATE TABLE stroke_reports (
    id               uuid PRIMARY KEY,
    seq              bigint GENERATED ALWAYS AS IDENTITY UNIQUE,
    reporter_digest  bytea NOT NULL CHECK (octet_length(reporter_digest) = 32),
    reason           text NOT NULL CHECK (reason IN ('sexual', 'violence', 'hate', 'illegal', 'spam', 'other')),
    block_author     boolean NOT NULL,
    status           text NOT NULL DEFAULT 'open' CHECK (status IN ('open', 'actioned', 'dismissed')),
    created_at       timestamptz NOT NULL DEFAULT now(),
    decided_at       timestamptz,
    CHECK ((status = 'open') = (decided_at IS NULL))
);

CREATE INDEX stroke_reports_reporter_recent_idx ON stroke_reports (reporter_digest, created_at DESC);
CREATE INDEX stroke_reports_open_idx ON stroke_reports (created_at) WHERE status = 'open';

CREATE TABLE stroke_report_items (
    report_id  uuid NOT NULL REFERENCES stroke_reports (id) ON DELETE CASCADE,
    stroke_id  uuid NOT NULL REFERENCES strokes (id),
    PRIMARY KEY (report_id, stroke_id)
);

CREATE INDEX stroke_report_items_stroke_idx ON stroke_report_items (stroke_id);

-- A stroke hidden by the report threshold, pending a moderator decision.
CREATE TABLE stroke_holds (
    stroke_id    uuid PRIMARY KEY REFERENCES strokes (id),
    held_at      timestamptz NOT NULL DEFAULT now(),
    released_at  timestamptz,
    outcome      text CHECK (outcome IN ('hidden', 'restored')),
    CHECK ((released_at IS NULL) = (outcome IS NULL))
);

CREATE TABLE banned_owners (
    owner_digest  bytea PRIMARY KEY CHECK (octet_length(owner_digest) = 32),
    created_at    timestamptz NOT NULL DEFAULT now()
);
