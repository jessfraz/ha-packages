-- One source message can describe several independently tracked parcels.
CREATE TABLE shared_sources (
    namespace TEXT NOT NULL,
    key TEXT NOT NULL,
    shipment_id TEXT NOT NULL REFERENCES shipments(id),
    PRIMARY KEY (namespace, key, shipment_id)
);
INSERT INTO shared_sources (namespace, key, shipment_id)
    SELECT namespace, key, shipment_id FROM sources;
DROP TABLE sources;
ALTER TABLE shared_sources RENAME TO sources;
CREATE INDEX sources_shipment ON sources (shipment_id);
