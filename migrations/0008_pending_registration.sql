ALTER TABLE shipments ADD COLUMN registration_state TEXT NOT NULL DEFAULT 'pending'
    CHECK (registration_state IN ('pending', 'registered', 'rejected'));
ALTER TABLE shipments ADD COLUMN registration_started_at INTEGER;
ALTER TABLE shipments ADD COLUMN last_error_code TEXT;
ALTER TABLE shipments ADD COLUMN last_provider_status INTEGER
    CHECK (last_provider_status BETWEEN 100 AND 599);
ALTER TABLE shipments ADD COLUMN last_provider_error_code TEXT;

UPDATE shipments SET registration_state = CASE
    WHEN tracker_id IS NOT NULL THEN 'registered' ELSE 'pending'
END,
registration_started_at = CASE WHEN tracker_id IS NULL THEN last_attempt_at END;

UPDATE shipments SET next_attempt_at = CASE
    WHEN last_error IS NOT NULL AND next_attempt_at > unixepoch() THEN next_attempt_at
    ELSE unixepoch()
END
WHERE registration_state = 'pending' AND archived = 0
    AND coalesce(override_status,carrier_status,'"expected"')
        NOT IN ('"delivered"','"return_to_sender"','"cancelled"');
