-- Order delivery calendar history by actual delivery date, estimates by ETA.
-- A status override is authoritative, but its set time is not an actual date.
-- Missing or invalid actual dates must never fall back to an ETA.
ALTER TABLE shipments ADD COLUMN calendar_sort_key INTEGER
    GENERATED ALWAYS AS (coalesce(unixepoch(
        CASE WHEN coalesce(override_status, carrier_status, '"expected"') = '"delivered"'
            THEN json_extract(carrier_tracking, '$.delivered_at')
            ELSE json_extract(carrier_tracking, '$.estimated_delivery_at')
        END
    ), 9223372036854775807)) VIRTUAL;
CREATE INDEX shipments_direction_calendar_delivery
    ON shipments (archived, direction, calendar_sort_key, id);
