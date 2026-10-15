-- Tracker identities and pending registration attempts belong to one provider.
-- Existing shipments were all registered through EasyPost.
ALTER TABLE shipments ADD COLUMN tracking_provider TEXT NOT NULL DEFAULT 'easypost';

DROP INDEX shipments_sync_due;
CREATE INDEX shipments_sync_due ON shipments (tracking_provider, next_attempt_at, id)
    WHERE archived = 0 AND tracking_number IS NOT NULL;

-- Clarify existing unsupported registrations without retrying or changing
-- tracking identity, scheduling, counters, or native provider diagnostics.
UPDATE shipments
SET last_error = 'Tracking is unavailable from the configured provider (EasyPost): no supported carrier was found. This does not mean the tracking number is invalid. Check provider support and the selected carrier before changing the number.'
WHERE last_error_code = 'tracking_unsupported'
  AND last_provider_error_code = 'TRACKER.NO_CARRIER_FOR_CODE';
