CREATE TABLE pickup_sites (
    id TEXT PRIMARY KEY NOT NULL,
    revision INTEGER NOT NULL DEFAULT 1 CHECK (revision > 0),
    is_default INTEGER NOT NULL CHECK (is_default IN (0,1)),
    details TEXT NOT NULL
);
CREATE UNIQUE INDEX pickup_sites_default ON pickup_sites(is_default) WHERE is_default=1;

CREATE TABLE pickup_parcels (
    carrier TEXT NOT NULL CHECK (carrier IN ('usps','ups','fedex')),
    tracking TEXT NOT NULL,
    details TEXT NOT NULL,
    PRIMARY KEY(carrier,tracking)
);
CREATE INDEX pickup_parcels_tracking ON pickup_parcels(tracking,carrier);

CREATE TABLE pickup_batches (
    id TEXT PRIMARY KEY NOT NULL,
    actor TEXT NOT NULL,
    operation_key TEXT NOT NULL,
    fingerprint BLOB NOT NULL,
    created_at INTEGER NOT NULL DEFAULT (unixepoch()),
    UNIQUE(actor,operation_key)
);

CREATE TABLE pickup_jobs (
    id TEXT PRIMARY KEY NOT NULL,
    batch_id TEXT NOT NULL REFERENCES pickup_batches(id),
    group_key TEXT NOT NULL,
    account_fingerprint BLOB NOT NULL,
    revision INTEGER NOT NULL DEFAULT 1 CHECK (revision > 0),
    state TEXT NOT NULL DEFAULT 'pending' CHECK (state IN ('pending','dispatching','scheduled','cancel_pending','cancelling','cancelled','rejected','unknown')),
    accept_fee INTEGER NOT NULL CHECK (accept_fee IN (0,1)),
    plan TEXT NOT NULL,
    carrier TEXT GENERATED ALWAYS AS (json_extract(plan,'$.carrier')) STORED NOT NULL,
    confirmation_number TEXT GENERATED ALWAYS AS (json_extract(confirmation,'$.number')) STORED,
    pickup_date TEXT GENERATED ALWAYS AS (coalesce(json_extract(confirmation,'$.pickup_date'),json_extract(plan,'$.date'))) STORED,
    offer TEXT,
    confirmation TEXT,
    error TEXT,
    cancel_actor TEXT,
    cancel_key TEXT,
    created_at INTEGER NOT NULL DEFAULT (unixepoch()),
    updated_at INTEGER NOT NULL DEFAULT (unixepoch()),
    UNIQUE(batch_id,group_key)
);
CREATE INDEX pickup_jobs_confirmation ON pickup_jobs(confirmation_number,carrier) WHERE confirmation_number IS NOT NULL;
CREATE INDEX pickup_jobs_calendar ON pickup_jobs(state,pickup_date,id);
CREATE INDEX pickup_jobs_inflight ON pickup_jobs(updated_at) WHERE state IN ('dispatching','cancelling');
CREATE INDEX pickup_jobs_pending ON pickup_jobs(id) WHERE state IN ('pending','cancel_pending');

CREATE TABLE pickup_batch_items (
    batch_id TEXT NOT NULL REFERENCES pickup_batches(id),
    pickup_id TEXT NOT NULL REFERENCES pickup_jobs(id),
    PRIMARY KEY(batch_id,pickup_id)
);

CREATE TABLE pickup_labels (
    pickup_id TEXT NOT NULL REFERENCES pickup_jobs(id),
    carrier TEXT NOT NULL,
    tracking TEXT NOT NULL,
    PRIMARY KEY(pickup_id,tracking)
);
CREATE INDEX pickup_labels_tracking ON pickup_labels(tracking,carrier,pickup_id);

-- A label cannot be dispatched again under a new command/idempotency key.
-- Claims are released only after a definite rejection or confirmed cancellation.
CREATE TABLE pickup_label_claims (
    carrier TEXT NOT NULL,
    tracking TEXT NOT NULL,
    pickup_id TEXT NOT NULL REFERENCES pickup_jobs(id),
    PRIMARY KEY(carrier,tracking)
);
CREATE INDEX pickup_label_claims_job ON pickup_label_claims(pickup_id);

CREATE TABLE pickup_intent_claims (
    fingerprint BLOB PRIMARY KEY NOT NULL,
    pickup_id TEXT NOT NULL UNIQUE REFERENCES pickup_jobs(id)
);

CREATE TABLE pickup_reconciliations (
    pickup_id TEXT NOT NULL REFERENCES pickup_jobs(id),
    revision INTEGER NOT NULL,
    actor TEXT NOT NULL,
    assertion TEXT NOT NULL,
    created_at INTEGER NOT NULL DEFAULT (unixepoch()),
    PRIMARY KEY(pickup_id,revision)
);

CREATE TABLE pickup_cooldowns (
    account_fingerprint BLOB PRIMARY KEY NOT NULL,
    not_before INTEGER NOT NULL
);
