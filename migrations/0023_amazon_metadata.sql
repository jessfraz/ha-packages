-- The first created snapshot owns the generated title. Never reinterpret an
-- adopted record or a title that an owner changed, even if later restored.
CREATE TEMP TABLE amazon_generated_titles (id TEXT PRIMARY KEY NOT NULL);
INSERT INTO amazon_generated_titles(id)
SELECT shipment.id
FROM shipments AS shipment
JOIN events AS created ON created.id = (
    SELECT min(id) FROM events
    WHERE shipment_id=shipment.id AND kind='"created"'
)
WHERE shipment.tracking_provider='amazon_account'
  AND created.actor_id='amazon_account'
  AND created.snapshot IS NOT NULL
  AND json_extract(created.snapshot,'$.title')=shipment.title
  AND substr(shipment.title,1,8)<>'Amazon: '
  -- Keep the eight-byte prefix within the 500-byte title contract, without
  -- truncating an existing product name or splitting a UTF-8 character.
  AND length(CAST(shipment.title AS BLOB))<=492
  AND NOT EXISTS (
      SELECT 1 FROM events AS edited
      WHERE edited.shipment_id=shipment.id AND edited.id>created.id
        AND edited.snapshot IS NOT NULL
        AND json_extract(edited.snapshot,'$.title') IS NOT shipment.title
  );

-- A title correction is not a second delivery notification. Prior event
-- snapshots and their original names remain historical evidence.
INSERT INTO events(shipment_id,kind,actor_id,detail)
SELECT id,'"updated"','system','Amazon Account title prefixed'
FROM amazon_generated_titles;
UPDATE shipments
SET title='Amazon: ' || title,revision=revision+1,
    updated_at=strftime('%Y-%m-%dT%H:%M:%fZ','now')
WHERE id IN (SELECT id FROM amazon_generated_titles);
DROP TABLE amazon_generated_titles;

-- Queue one normal saved-page read, including naturally delivered parcels.
-- Keep future pacing, errors, counters, and current leases intact. No delivery dates
-- or registration resets belong in a metadata migration.
UPDATE shipments
SET next_attempt_at=CASE
    WHEN lease_until>unixepoch() OR next_attempt_at>unixepoch() THEN next_attempt_at
    ELSE unixepoch()
END
WHERE tracking_provider='amazon_account' AND archived=0
  AND tracking_number IS NOT NULL AND tracker_id IS NOT NULL
  AND registration_state='registered'
  AND coalesce(override_status,'') NOT IN ('"delivered"','"return_to_sender"','"cancelled"')
  AND EXISTS (SELECT 1 FROM amazon_parcels WHERE shipment_id=shipments.id)
  AND json_extract(carrier_tracking,'$.delivery_date') IS NULL
  AND (
      (coalesce(override_status,carrier_status,'"expected"')='"delivered"'
       AND json_extract(carrier_tracking,'$.delivered_at') IS NULL)
      OR
      (coalesce(override_status,carrier_status,'"expected"')
           NOT IN ('"delivered"','"return_to_sender"','"cancelled"')
       AND json_extract(carrier_tracking,'$.estimated_delivery_at') IS NULL
       AND json_extract(carrier_tracking,'$.estimated_delivery_window') IS NULL)
  );
