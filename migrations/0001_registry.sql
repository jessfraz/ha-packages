CREATE TABLE shipments (
    id TEXT PRIMARY KEY NOT NULL,
    revision INTEGER NOT NULL DEFAULT 1,
    title TEXT NOT NULL,
    merchant TEXT,
    order_reference TEXT,
    tracking_carrier TEXT,
    tracking_number TEXT,
    tracking_generation INTEGER NOT NULL DEFAULT 0,
    carrier_status TEXT,
    override_status TEXT,
    override_reason TEXT,
    override_actor TEXT,
    override_at TEXT,
    archived INTEGER NOT NULL DEFAULT 0 CHECK (archived IN (0, 1)),
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    tracker_id TEXT,
    last_success_at TEXT,
    last_error TEXT,
    next_attempt_at INTEGER,
    attempts INTEGER NOT NULL DEFAULT 0,
    lease_until INTEGER,
    lease_token TEXT
);
CREATE UNIQUE INDEX shipments_active_tracking ON shipments (
    tracking_number, coalesce(tracking_carrier, '')
) WHERE archived = 0 AND tracking_number IS NOT NULL;
CREATE INDEX shipments_order_reference ON shipments (order_reference, id);
CREATE INDEX shipments_sync_due ON shipments (next_attempt_at, id)
    WHERE archived = 0 AND tracking_number IS NOT NULL;
CREATE INDEX shipments_visible ON shipments (archived, id);

CREATE TABLE sources (
    namespace TEXT NOT NULL,
    key TEXT NOT NULL,
    shipment_id TEXT NOT NULL REFERENCES shipments(id),
    PRIMARY KEY (namespace, key)
);
CREATE INDEX sources_shipment ON sources (shipment_id);

CREATE TABLE events (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    shipment_id TEXT NOT NULL REFERENCES shipments(id),
    kind TEXT NOT NULL,
    actor_id TEXT NOT NULL,
    at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    detail TEXT NOT NULL,
    snapshot TEXT
);
CREATE INDEX events_shipment ON events (shipment_id, id);

CREATE TABLE idempotency (
    actor_id TEXT NOT NULL,
    key TEXT NOT NULL,
    fingerprint TEXT NOT NULL,
    response TEXT NOT NULL,
    PRIMARY KEY (actor_id, key)
);
