-- Stored ledger scripts (FR-66): immutable versions under a name, numbered from 1.
CREATE TABLE scripts (
    name       TEXT NOT NULL CHECK (name ~ '^[a-z0-9_-]{1,64}$'),
    version    INTEGER NOT NULL CHECK (version > 0),
    source     TEXT NOT NULL CHECK (octet_length(source) <= 65536),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (name, version)
);

-- A registration replays as the script version it created.
ALTER TABLE idempotency_keys
    ADD COLUMN script_name    TEXT,
    ADD COLUMN script_version INTEGER,
    ADD FOREIGN KEY (script_name, script_version) REFERENCES scripts (name, version);
