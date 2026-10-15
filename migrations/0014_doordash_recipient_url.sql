-- Recipient URLs carry a separate, opaque DoorDash delivery identity. They
-- cannot be reconstructed from the tracking number or shared across attempts.
ALTER TABLE shipments ADD COLUMN recipient_url TEXT;
ALTER TABLE tracking_verifications ADD COLUMN requested_recipient_url TEXT;
CREATE INDEX state_events_shipment ON state_events(shipment_id);

CREATE TEMP TABLE unlinked_doordash (id TEXT PRIMARY KEY NOT NULL);
INSERT INTO unlinked_doordash SELECT id FROM shipments
WHERE lower(tracking_carrier) = 'doordash'
  AND (recipient_url IS NULL OR trim(recipient_url,
    char(9,10,11,12,13,32,133,160,5760,8192,8193,8194,8195,8196,8197,8198,8199,8200,8201,8202,8232,8233,8239,8287,12288)) = '');

DELETE FROM admission_sources WHERE (namespace,key) IN
    (SELECT namespace,key FROM sources WHERE shipment_id IN (SELECT id FROM unlinked_doordash));
DELETE FROM sources WHERE shipment_id IN (SELECT id FROM unlinked_doordash);
DELETE FROM state_events WHERE shipment_id IN (SELECT id FROM unlinked_doordash);
DELETE FROM events WHERE shipment_id IN (SELECT id FROM unlinked_doordash);
DELETE FROM admission_requests WHERE (actor_id,operation_key) IN
    (SELECT actor_id,'create:' || key FROM idempotency WHERE json_extract(response,'$.id') IN (SELECT id FROM unlinked_doordash)
     OR (lower(json_extract(response,'$.tracking.carrier'))='doordash'
         AND coalesce(trim(json_extract(response,'$.tracking.recipient_url')),'')=''));
DELETE FROM idempotency WHERE json_extract(response,'$.id') IN (SELECT id FROM unlinked_doordash)
    OR (lower(json_extract(response,'$.tracking.carrier'))='doordash'
        AND coalesce(trim(json_extract(response,'$.tracking.recipient_url')),'')='');
DELETE FROM shipments WHERE id IN (SELECT id FROM unlinked_doordash);
DROP TABLE unlinked_doordash;

CREATE TRIGGER shipments_doordash_link_insert BEFORE INSERT ON shipments
WHEN lower(NEW.tracking_carrier)='doordash'
  AND (NEW.recipient_url IS NULL OR trim(NEW.recipient_url,
    char(9,10,11,12,13,32,133,160,5760,8192,8193,8194,8195,8196,8197,8198,8199,8200,8201,8202,8232,8233,8239,8287,12288))='')
BEGIN SELECT RAISE(ABORT,'DoorDash requires the original recipient URL'); END;
CREATE TRIGGER shipments_doordash_link_update BEFORE UPDATE OF tracking_carrier,recipient_url ON shipments
WHEN lower(NEW.tracking_carrier)='doordash'
  AND (NEW.recipient_url IS NULL OR trim(NEW.recipient_url,
    char(9,10,11,12,13,32,133,160,5760,8192,8193,8194,8195,8196,8197,8198,8199,8200,8201,8202,8232,8233,8239,8287,12288))='')
BEGIN SELECT RAISE(ABORT,'DoorDash requires the original recipient URL'); END;

-- Keep prior ownership in the audit trail. An EasyPost ID is never passed to
-- AfterShip; its normal reconciliation finds an existing AfterShip import.
INSERT INTO events(shipment_id,kind,actor_id,detail)
SELECT id,'"updated"','system',
    'Tracking moved to AfterShip; previous EasyPost tracker: ' || tracker_id
FROM shipments WHERE lower(tracking_carrier) IN ('speedx','sway')
  AND tracking_provider='easypost' AND tracker_id IS NOT NULL;
UPDATE shipments SET tracking_provider='aftership',tracking_generation=tracking_generation+1,
    revision=revision+1,updated_at=strftime('%Y-%m-%dT%H:%M:%fZ','now'),
    tracker_id=NULL,tracking_url=NULL,registration_state='pending',registration_started_at=NULL,
    last_success_at=NULL,last_attempt_at=NULL,last_error=NULL,last_error_code=NULL,
    last_provider_status=NULL,last_provider_error_code=NULL,attempts=0,
    lease_until=NULL,lease_token=NULL,
    next_attempt_at=CASE WHEN archived=0 AND coalesce(override_status,carrier_status,'"expected"')
        NOT IN ('"delivered"','"return_to_sender"','"cancelled"') THEN unixepoch() END
WHERE lower(tracking_carrier) IN ('speedx','sway') AND tracking_provider='easypost';
