-- Manifests need only the point count. jsonb_array_length(points) detoasted every
-- member's whole geometry before the first manifest chunk; a stored copy keeps the
-- manifest pass on the heap tuple. Strokes are immutable, so it never goes stale.
ALTER TABLE strokes
    ADD COLUMN point_count integer GENERATED ALWAYS AS (jsonb_array_length(points)) STORED;
