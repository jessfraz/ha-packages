ALTER TABLE amazon_discovery ADD COLUMN last_finished_at INTEGER;
ALTER TABLE amazon_discovery ADD COLUMN last_complete_at INTEGER;
ALTER TABLE amazon_discovery ADD COLUMN last_outcome TEXT
    CHECK (last_outcome IN ('complete', 'partial', 'blocked', 'failed'));
ALTER TABLE amazon_discovery ADD COLUMN last_error_code TEXT;
ALTER TABLE amazon_discovery ADD COLUMN has_more INTEGER NOT NULL DEFAULT 0;
ALTER TABLE amazon_discovery ADD COLUMN deferred_parcels INTEGER NOT NULL DEFAULT 0;
ALTER TABLE amazon_discovery ADD COLUMN deferred_store INTEGER NOT NULL DEFAULT 0;
