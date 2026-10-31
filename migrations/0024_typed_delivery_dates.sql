-- A civil delivery date participates in native projections without becoming a
-- fabricated carrier scan instant. An actual timestamp always takes priority.
ALTER TABLE shipments ADD COLUMN provider_delivered_on TEXT
    GENERATED ALWAYS AS (
        CASE WHEN unixepoch(json_extract(carrier_tracking, '$.delivered_at')) IS NULL
              AND json_extract(carrier_tracking, '$.delivery_date.kind') = 'delivered'
            THEN json_extract(carrier_tracking, '$.delivery_date.date')
        END
    ) VIRTUAL;

DROP INDEX shipments_expected_delivery;
DROP INDEX shipments_direction_expected_delivery;
DROP INDEX shipments_tracking_issues;
DROP INDEX shipments_delivered_date;
DROP INDEX shipments_direction_delivered_date;
DROP INDEX shipments_direction_calendar_delivery;
ALTER TABLE shipments DROP COLUMN expected_sort_key;
ALTER TABLE shipments DROP COLUMN delivered_sort_key;
ALTER TABLE shipments DROP COLUMN calendar_sort_key;

-- Epoch values here are stable ordering keys only. Public date-only values
-- retain their civil precision and timezone in delivery_date.
ALTER TABLE shipments ADD COLUMN expected_sort_key INTEGER
    GENERATED ALWAYS AS (coalesce(
        unixepoch(json_extract(carrier_tracking, '$.estimated_delivery_at')),
        unixepoch(CASE json_extract(carrier_tracking, '$.delivery_date.kind')
            WHEN 'estimated' THEN json_extract(carrier_tracking, '$.delivery_date.date')
            WHEN 'estimated_range' THEN json_extract(carrier_tracking, '$.delivery_date.start')
        END),
        9223372036854775807
    )) VIRTUAL;
ALTER TABLE shipments ADD COLUMN delivered_sort_key INTEGER
    GENERATED ALWAYS AS (coalesce(
        CASE WHEN coalesce(override_status, carrier_status) = '"delivered"' THEN
            coalesce(
                -unixepoch(json_extract(carrier_tracking, '$.delivered_at')),
                -unixepoch(provider_delivered_on),
                CASE WHEN override_status = '"delivered"' THEN -unixepoch(override_at) END
            )
        END,
        9223372036854775807
    )) VIRTUAL;
ALTER TABLE shipments ADD COLUMN calendar_sort_key INTEGER
    GENERATED ALWAYS AS (coalesce(
        CASE WHEN coalesce(override_status, carrier_status, '"expected"') = '"delivered"'
            THEN coalesce(
                unixepoch(json_extract(carrier_tracking, '$.delivered_at')),
                unixepoch(provider_delivered_on)
            )
            ELSE coalesce(
                unixepoch(json_extract(carrier_tracking, '$.estimated_delivery_window.start')),
                unixepoch(json_extract(carrier_tracking, '$.estimated_delivery_at')),
                unixepoch(CASE json_extract(carrier_tracking, '$.delivery_date.kind')
                    WHEN 'estimated' THEN json_extract(carrier_tracking, '$.delivery_date.date')
                    WHEN 'estimated_range' THEN json_extract(carrier_tracking, '$.delivery_date.start')
                END)
            )
        END,
        9223372036854775807
    )) VIRTUAL;

CREATE INDEX shipments_expected_delivery ON shipments (archived, expected_sort_key, id);
CREATE INDEX shipments_direction_expected_delivery
    ON shipments (archived, direction, expected_sort_key, id);
CREATE INDEX shipments_tracking_issues ON shipments (archived, expected_sort_key, id)
    WHERE last_error IS NOT NULL;
CREATE INDEX shipments_delivered_date ON shipments (archived, delivered_sort_key, id);
CREATE INDEX shipments_direction_delivered_date
    ON shipments (archived, direction, delivered_sort_key, id);
CREATE INDEX shipments_direction_calendar_delivery
    ON shipments (archived, direction, calendar_sort_key, id);
CREATE INDEX shipments_provider_delivered_on ON shipments (archived, provider_delivered_on, id)
    WHERE provider_delivered_on IS NOT NULL;
