-- Retail-account tracking was historically labeled as merchant MCF. Provider
-- ownership is authoritative here; genuine MCF and other providers stay alone.
-- Preserve prior snapshots and avoid publishing a duplicate delivery event.
INSERT INTO events(shipment_id,kind,actor_id,detail)
SELECT id,'"updated"','system',
    'Carrier corrected to Amazon Account; tracking ownership preserved'
FROM shipments
WHERE lower(tracking_carrier)='amazonmcf' AND tracking_provider='amazon_account';

UPDATE shipments
SET tracking_carrier='amazonaccount',revision=revision+1,
    updated_at=strftime('%Y-%m-%dT%H:%M:%fZ','now'),
    tracking_generation=tracking_generation+1,lease_token=NULL,lease_until=NULL
WHERE lower(tracking_carrier)='amazonmcf' AND tracking_provider='amazon_account';
