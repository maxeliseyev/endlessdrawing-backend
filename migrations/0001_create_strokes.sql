CREATE TABLE strokes (
    id            uuid PRIMARY KEY,
    -- Keep JSON numbers exactly representable by JavaScript clients.
    seq           bigint GENERATED ALWAYS AS IDENTITY (MAXVALUE 9007199254740991) UNIQUE,
    owner_digest  bytea NOT NULL CHECK (octet_length(owner_digest) = 32),
    points        jsonb NOT NULL CHECK (
        jsonb_typeof(points) = 'array' AND jsonb_array_length(points) > 0
    ),
    brush         text NOT NULL CHECK (brush IN ('round', 'square')),
    width         double precision NOT NULL CHECK (width BETWEEN 1 AND 64),
    color         text NOT NULL CHECK (color ~ '^#[0-9A-Fa-f]{6}$'),
    opacity       double precision NOT NULL CHECK (opacity > 0 AND opacity <= 1),
    bounds        box NOT NULL,
    created_at    timestamptz NOT NULL DEFAULT now(),
    deleted_at    timestamptz
);

CREATE INDEX strokes_visible_bounds_gist
    ON strokes USING gist (bounds)
    WHERE deleted_at IS NULL;

CREATE INDEX strokes_owner_recent_idx
    ON strokes (owner_digest, seq DESC)
    WHERE deleted_at IS NULL;
