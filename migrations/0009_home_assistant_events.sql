-- Preserve the transitions that happened between Home Assistant polls. The
-- source event and its notification commit together, including carrier writes.
CREATE TABLE state_events (
    id INTEGER PRIMARY KEY REFERENCES events(id) ON DELETE CASCADE,
    shipment_id TEXT NOT NULL REFERENCES shipments(id),
    event_type TEXT NOT NULL CHECK (event_type IN ('out_for_delivery', 'delivered', 'exception')),
    status TEXT NOT NULL,
    title TEXT NOT NULL,
    carrier TEXT,
    direction TEXT NOT NULL CHECK (direction IN ('inbound', 'outbound')),
    at TEXT NOT NULL
);

CREATE TRIGGER shipment_state_event AFTER INSERT ON events
WHEN NEW.snapshot IS NOT NULL
  AND coalesce(json_extract(NEW.snapshot, '$.archived'), 0) = 0
  AND json_extract(NEW.snapshot, '$.status') IN ('out_for_delivery', 'delivered', 'failure', 'error', 'return_to_sender')
  AND json_extract(NEW.snapshot, '$.status') IS NOT (
    SELECT json_extract(previous.snapshot, '$.status')
    FROM events AS previous
    WHERE previous.shipment_id = NEW.shipment_id
      AND previous.id < NEW.id
      AND previous.snapshot IS NOT NULL
    ORDER BY previous.id DESC
    LIMIT 1
  )
BEGIN
    INSERT INTO state_events (id, shipment_id, event_type, status, title, carrier, direction, at)
    VALUES (
        NEW.id,
        NEW.shipment_id,
        CASE json_extract(NEW.snapshot, '$.status')
            WHEN 'out_for_delivery' THEN 'out_for_delivery'
            WHEN 'delivered' THEN 'delivered'
            ELSE 'exception'
        END,
        json_extract(NEW.snapshot, '$.status'),
        json_extract(NEW.snapshot, '$.title'),
        json_extract(NEW.snapshot, '$.tracking.carrier'),
        coalesce(json_extract(NEW.snapshot, '$.direction'), 'inbound'),
        NEW.at
    );
END;
