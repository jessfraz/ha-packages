CREATE TABLE provider_connection_checks (
    provider TEXT PRIMARY KEY NOT NULL CHECK (provider IN ('easypost', 'aftership', 'doordash')),
    fingerprint BLOB NOT NULL CHECK (length(fingerprint) = 32),
    status TEXT NOT NULL CHECK (status IN ('pass', 'attention')),
    message TEXT NOT NULL,
    checked_at INTEGER NOT NULL CHECK (checked_at > 0)
);
