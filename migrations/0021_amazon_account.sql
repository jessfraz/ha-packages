-- Verified account parcel identity is separate from user-supplied provenance.
CREATE TABLE amazon_parcels (
    source_key TEXT PRIMARY KEY NOT NULL,
    shipment_id TEXT NOT NULL REFERENCES shipments(id),
    tracking_number TEXT NOT NULL
);
CREATE INDEX amazon_parcels_shipment ON amazon_parcels(shipment_id);

-- Discovery shares the normal polling policy but remains due even with no parcels.
CREATE TABLE amazon_discovery (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    next_attempt_at INTEGER NOT NULL DEFAULT (unixepoch()),
    attempts INTEGER NOT NULL DEFAULT 0,
    last_attempt_at INTEGER,
    lease_token TEXT,
    lease_until INTEGER
);
INSERT INTO amazon_discovery(id) VALUES (1);
