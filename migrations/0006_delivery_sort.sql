-- Persist indexes over normalized instants, rather than sorting only one API page.
-- The maximum integer puts unknown dates last; negated actual dates sort newest first.
ALTER TABLE shipments ADD COLUMN expected_sort_key INTEGER
    GENERATED ALWAYS AS (coalesce(
        unixepoch(json_extract(carrier_tracking, '$.estimated_delivery_at')),
        9223372036854775807
    )) VIRTUAL;
ALTER TABLE shipments ADD COLUMN delivered_sort_key INTEGER
    GENERATED ALWAYS AS (coalesce(-unixepoch(
        CASE WHEN coalesce(override_status, carrier_status) = '"delivered"' THEN
            coalesce(
                json_extract(carrier_tracking, '$.delivered_at'),
                CASE WHEN override_status = '"delivered"' THEN override_at END
            )
        END
    ), 9223372036854775807)) VIRTUAL;
CREATE INDEX shipments_expected_delivery ON shipments (archived, expected_sort_key, id);
CREATE INDEX shipments_delivered_date ON shipments (archived, delivered_sort_key, id);
