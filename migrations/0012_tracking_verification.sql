-- Admission attempts are not shipments. Keep uncertain provider writes durable
-- even when the caller disconnects before a verified shipment can be saved.
CREATE TABLE tracking_verifications (
    provider TEXT NOT NULL,
    tracking_number TEXT NOT NULL,
    requested_carrier TEXT,
    started_at INTEGER NOT NULL DEFAULT (unixepoch()),
    lease_token TEXT,
    lease_until INTEGER,
    attempts INTEGER NOT NULL DEFAULT 0,
    next_attempt_at INTEGER NOT NULL DEFAULT 0,
    tracker TEXT,
    verified_at INTEGER,
    failure TEXT,
    PRIMARY KEY (provider, tracking_number)
);

-- A failed admission still binds its idempotency key to its original input.
CREATE TABLE admission_requests (
    actor_id TEXT NOT NULL,
    operation_key TEXT NOT NULL,
    fingerprint TEXT NOT NULL,
    PRIMARY KEY (actor_id, operation_key)
);

-- Reserve source ownership before paid verification, without holding a
-- database transaction across the provider call.
CREATE TABLE admission_sources (
    namespace TEXT NOT NULL,
    key TEXT NOT NULL,
    actor_id TEXT NOT NULL,
    operation_key TEXT NOT NULL,
    lease_token TEXT NOT NULL,
    lease_until INTEGER NOT NULL,
    PRIMARY KEY (namespace, key)
);
