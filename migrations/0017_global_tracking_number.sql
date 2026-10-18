-- Archive status and carrier corrections never release a tracking number.
-- Duplicate legacy data deliberately fails this migration without deleting it.
DROP INDEX shipments_active_tracking;
CREATE UNIQUE INDEX shipments_tracking_number ON shipments (tracking_number);

-- Hold number ownership across provider I/O and the final local commit, even
-- when competing requests choose different providers. Abandoned claims expire.
CREATE TABLE admission_tracking (
    tracking_number TEXT PRIMARY KEY NOT NULL,
    lease_token TEXT NOT NULL,
    lease_until INTEGER NOT NULL
);
