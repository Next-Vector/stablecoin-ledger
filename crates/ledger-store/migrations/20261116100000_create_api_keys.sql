-- API keys (NFR-09). Only a SHA-256 hash of the secret is stored; the plaintext is shown once.
CREATE TABLE api_keys (
    key_id      TEXT PRIMARY KEY CHECK (key_id ~ '^[0-9a-f]{8}$'),
    name        TEXT NOT NULL CHECK (btrim(name) <> ''),
    role        TEXT NOT NULL CHECK (role IN ('read', 'write', 'admin')),
    secret_hash BYTEA NOT NULL CHECK (octet_length(secret_hash) = 32),
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    revoked_at  TIMESTAMPTZ
);
