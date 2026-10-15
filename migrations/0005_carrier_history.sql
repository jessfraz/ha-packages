ALTER TABLE shipments ADD COLUMN signature_required INTEGER
    CHECK (signature_required IN (0, 1));
ALTER TABLE shipments ADD COLUMN carrier_tracking TEXT NOT NULL DEFAULT '{}';

-- Fetch the newly retained carrier details once for existing tracked parcels,
-- including terminal records whose normal polling has already stopped. Keep
-- provider retry advice intact; the shared worker paces this enrichment queue.
UPDATE shipments
SET next_attempt_at = CASE
    WHEN last_error IS NOT NULL AND next_attempt_at > unixepoch()
        THEN next_attempt_at
    ELSE unixepoch()
END
WHERE archived = 0 AND tracking_number IS NOT NULL AND tracker_id IS NOT NULL;
