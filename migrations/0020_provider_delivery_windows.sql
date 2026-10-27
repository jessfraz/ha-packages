-- Keep verified delivery history authoritative. An explicit carrier range can
-- stand alone, and its start determines the stable calendar cursor position.
DROP INDEX shipments_direction_calendar_delivery;
ALTER TABLE shipments DROP COLUMN calendar_sort_key;
ALTER TABLE shipments ADD COLUMN calendar_sort_key INTEGER
    GENERATED ALWAYS AS (coalesce(unixepoch(
        CASE WHEN coalesce(override_status, carrier_status, '"expected"') = '"delivered"'
            THEN json_extract(carrier_tracking, '$.delivered_at')
            ELSE coalesce(
                json_extract(carrier_tracking, '$.estimated_delivery_window.start'),
                json_extract(carrier_tracking, '$.estimated_delivery_at')
            )
        END
    ), 9223372036854775807)) VIRTUAL;
CREATE INDEX shipments_direction_calendar_delivery
    ON shipments (archived, direction, calendar_sort_key, id);
