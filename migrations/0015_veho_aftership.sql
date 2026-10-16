-- Veho recipient tracking is available through AfterShip without the shipper's
-- EasyPost carrier credentials. Preserve old ownership for reconciliation audits.
INSERT INTO events(shipment_id,kind,actor_id,detail)
SELECT id,'"updated"','system',
    'Tracking moved to AfterShip; previous EasyPost tracker: ' || tracker_id
FROM shipments WHERE lower(tracking_carrier)='veho'
  AND tracking_provider='easypost' AND tracker_id IS NOT NULL;

-- Never send an EasyPost tracker ID to AfterShip. Its normal registration path
-- reconciles existing AfterShip imports before considering a new registration.
UPDATE shipments SET tracking_provider='aftership',tracking_generation=tracking_generation+1,
    revision=revision+1,updated_at=strftime('%Y-%m-%dT%H:%M:%fZ','now'),
    tracker_id=NULL,tracking_url=NULL,registration_state='pending',registration_started_at=NULL,
    last_success_at=NULL,last_attempt_at=NULL,last_error=NULL,last_error_code=NULL,
    last_provider_status=NULL,last_provider_error_code=NULL,attempts=0,
    lease_until=NULL,lease_token=NULL,
    next_attempt_at=CASE WHEN archived=0 AND coalesce(override_status,carrier_status,'"expected"')
        NOT IN ('"delivered"','"return_to_sender"','"cancelled"') THEN unixepoch() END
WHERE lower(tracking_carrier)='veho' AND tracking_provider='easypost';
