-- Spray v1 and the -45 degree chisel nib are reconstructed by clients from
-- id/points/width; no extra columns. Existing rows remain valid.
ALTER TABLE strokes DROP CONSTRAINT strokes_brush_check;
ALTER TABLE strokes ADD CONSTRAINT strokes_brush_check
    CHECK (brush IN ('round', 'square', 'chisel', 'spray'));
