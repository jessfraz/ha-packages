CREATE TABLE agent_credentials (
    id TEXT PRIMARY KEY NOT NULL,
    name TEXT NOT NULL,
    token_digest BLOB NOT NULL UNIQUE CHECK (length(token_digest) = 32),
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);
