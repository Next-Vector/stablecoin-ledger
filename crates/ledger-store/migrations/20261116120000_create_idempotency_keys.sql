-- Idempotency keys (FR-10, FR-11) for every mutating API request. A key is recorded in the same
-- DB transaction as the write it guards, so a retry can never repeat a committed write, and the
-- primary key makes a concurrent duplicate fail and roll back. A replay returns the recorded
-- resource; request_hash detects reuse of a key for a different request.
CREATE TABLE idempotency_keys (
    key            TEXT PRIMARY KEY CHECK (key ~ '^[!-~]{1,255}$'),
    request_hash   BYTEA NOT NULL CHECK (octet_length(request_hash) = 32),
    transaction_id BIGINT REFERENCES transactions (id),
    hold_id        BIGINT REFERENCES holds (id),
    created_at     TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Superseded by idempotency_keys, which also covers requests that write no transaction.
ALTER TABLE transactions DROP COLUMN idempotency_key, DROP COLUMN request_hash;
