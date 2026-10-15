ALTER TABLE shipments ADD COLUMN tracking_url TEXT;

CREATE TEMP TABLE shipments_without_tracking (id TEXT PRIMARY KEY NOT NULL);
INSERT INTO shipments_without_tracking
SELECT id FROM shipments
WHERE tracking_number IS NULL OR trim(tracking_number, char(9,10,11,12,13,32,133,160,5760,8192,8193,8194,8195,8196,8197,8198,8199,8200,8201,8202,8232,8233,8239,8287,12288)) = '';

DELETE FROM sources WHERE shipment_id IN (SELECT id FROM shipments_without_tracking);
DELETE FROM events WHERE shipment_id IN (SELECT id FROM shipments_without_tracking);
-- Remove replay responses too: a retry must never resurrect a deleted record or
-- return a historical create response that does not contain a tracking number.
DELETE FROM idempotency
WHERE json_extract(response, '$.id') IN (SELECT id FROM shipments_without_tracking)
   OR json_extract(response, '$.tracking.number') IS NULL
   OR trim(json_extract(response, '$.tracking.number'), char(9,10,11,12,13,32,133,160,5760,8192,8193,8194,8195,8196,8197,8198,8199,8200,8201,8202,8232,8233,8239,8287,12288)) = '';
DELETE FROM shipments WHERE id IN (SELECT id FROM shipments_without_tracking);
DROP TABLE shipments_without_tracking;

CREATE TRIGGER shipments_require_tracking_insert
BEFORE INSERT ON shipments
WHEN NEW.tracking_number IS NULL OR trim(NEW.tracking_number, char(9,10,11,12,13,32,133,160,5760,8192,8193,8194,8195,8196,8197,8198,8199,8200,8201,8202,8232,8233,8239,8287,12288)) = ''
BEGIN
    SELECT RAISE(ABORT, 'tracking number is required');
END;

CREATE TRIGGER shipments_require_tracking_update
BEFORE UPDATE OF tracking_number ON shipments
WHEN NEW.tracking_number IS NULL OR trim(NEW.tracking_number, char(9,10,11,12,13,32,133,160,5760,8192,8193,8194,8195,8196,8197,8198,8199,8200,8201,8202,8232,8233,8239,8287,12288)) = ''
BEGIN
    SELECT RAISE(ABORT, 'tracking number is required');
END;
