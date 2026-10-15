ALTER TABLE shipments ADD COLUMN direction TEXT NOT NULL DEFAULT 'inbound'
    CHECK (direction IN ('inbound', 'outbound'));
CREATE INDEX shipments_direction_expected_delivery
    ON shipments (archived, direction, expected_sort_key, id);
CREATE INDEX shipments_direction_delivered_date
    ON shipments (archived, direction, delivered_sort_key, id);
