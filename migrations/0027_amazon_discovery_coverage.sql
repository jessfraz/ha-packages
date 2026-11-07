ALTER TABLE amazon_discovery ADD COLUMN coverage_incomplete INTEGER NOT NULL DEFAULT 0
    CHECK (coverage_incomplete IN (0, 1));

UPDATE amazon_discovery
SET coverage_incomplete = coalesce(last_outcome = 'partial', 0) AND has_more = 0
WHERE id = 1;
