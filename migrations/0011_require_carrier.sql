-- A carrier is part of every tracking identity. Remove incomplete imports and
-- their replay records rather than inventing a carrier or registering them.
CREATE TEMP TABLE shipments_without_carrier (id TEXT PRIMARY KEY NOT NULL);
INSERT INTO shipments_without_carrier
SELECT id FROM shipments
WHERE tracking_carrier IS NULL OR trim(tracking_carrier, char(9,10,11,12,13,32,133,160,5760,8192,8193,8194,8195,8196,8197,8198,8199,8200,8201,8202,8232,8233,8239,8287,12288)) = '';

DELETE FROM sources WHERE shipment_id IN (SELECT id FROM shipments_without_carrier);
DELETE FROM state_events WHERE shipment_id IN (SELECT id FROM shipments_without_carrier);
DELETE FROM events WHERE shipment_id IN (SELECT id FROM shipments_without_carrier);
DELETE FROM idempotency
WHERE json_extract(response, '$.id') IN (SELECT id FROM shipments_without_carrier)
   OR json_extract(response, '$.tracking.carrier') IS NULL
   OR trim(json_extract(response, '$.tracking.carrier'), char(9,10,11,12,13,32,133,160,5760,8192,8193,8194,8195,8196,8197,8198,8199,8200,8201,8202,8232,8233,8239,8287,12288)) = '';
DELETE FROM shipments WHERE id IN (SELECT id FROM shipments_without_carrier);
DROP TABLE shipments_without_carrier;

DROP INDEX shipments_active_tracking;
CREATE UNIQUE INDEX shipments_active_tracking ON shipments (tracking_number, tracking_carrier)
    WHERE archived = 0 AND tracking_number IS NOT NULL;

CREATE INDEX shipments_tracking_issues ON shipments (archived, expected_sort_key, id)
    WHERE last_error IS NOT NULL;
CREATE INDEX shipments_carrier_delivered_at ON shipments (
    archived, unixepoch(json_extract(carrier_tracking,'$.delivered_at')), id
);

CREATE TRIGGER shipments_require_carrier_insert
BEFORE INSERT ON shipments
WHEN NEW.tracking_carrier IS NULL OR trim(NEW.tracking_carrier, char(9,10,11,12,13,32,133,160,5760,8192,8193,8194,8195,8196,8197,8198,8199,8200,8201,8202,8232,8233,8239,8287,12288)) = ''
BEGIN
    SELECT RAISE(ABORT, 'carrier is required');
END;

CREATE TRIGGER shipments_require_carrier_update
BEFORE UPDATE OF tracking_carrier ON shipments
WHEN NEW.tracking_carrier IS NULL OR trim(NEW.tracking_carrier, char(9,10,11,12,13,32,133,160,5760,8192,8193,8194,8195,8196,8197,8198,8199,8200,8201,8202,8232,8233,8239,8287,12288)) = ''
BEGIN
    SELECT RAISE(ABORT, 'carrier is required');
END;
