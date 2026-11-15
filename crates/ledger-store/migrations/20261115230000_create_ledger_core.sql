-- Core ledger schema (spec §9). transactions and postings are the append-only source of truth;
-- balances is a materialized view of them, updated in the same DB transaction.
-- Amounts are NUMERIC(39,0) to cover the full u128 range; balances are signed NUMERIC(40,0).

CREATE TABLE transactions (
    id              BIGSERIAL PRIMARY KEY,
    idempotency_key TEXT UNIQUE,
    request_hash    BYTEA,
    reverts         BIGINT UNIQUE REFERENCES transactions (id),
    metadata        JSONB NOT NULL DEFAULT '{}',
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE postings (
    tx_id       BIGINT NOT NULL REFERENCES transactions (id),
    idx         SMALLINT NOT NULL,
    source      TEXT NOT NULL,
    destination TEXT NOT NULL,
    asset       TEXT NOT NULL,
    amount      NUMERIC(39, 0) NOT NULL CHECK (amount > 0),
    PRIMARY KEY (tx_id, idx),
    CHECK (source <> destination)
);

-- History and point-in-time balance queries filter postings by account and asset.
CREATE INDEX postings_source_asset_idx ON postings (source, asset, tx_id);
CREATE INDEX postings_destination_asset_idx ON postings (destination, asset, tx_id);

CREATE TABLE balances (
    account TEXT NOT NULL,
    asset   TEXT NOT NULL,
    balance NUMERIC(40, 0) NOT NULL DEFAULT 0,
    version BIGINT NOT NULL DEFAULT 0,
    PRIMARY KEY (account, asset)
);

CREATE TABLE accounts (
    address        TEXT PRIMARY KEY,
    metadata       JSONB NOT NULL DEFAULT '{}',
    allow_negative BOOLEAN NOT NULL DEFAULT false,
    frozen         BOOLEAN NOT NULL DEFAULT false
);

-- Hold ids are sequential like ledger-core's HoldId; the technical account is holds:{id}.
CREATE TABLE holds (
    id         BIGSERIAL PRIMARY KEY,
    owner      TEXT NOT NULL,
    asset      TEXT NOT NULL,
    amount     NUMERIC(39, 0) NOT NULL CHECK (amount > 0),
    status     TEXT NOT NULL CHECK (status IN ('active', 'captured', 'voided', 'expired')),
    expires_at TIMESTAMPTZ
);

CREATE INDEX holds_active_expiry_idx ON holds (expires_at) WHERE status = 'active';

-- The journal is immutable: corrections are reversals, never edits.
CREATE FUNCTION forbid_journal_modification() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    RAISE EXCEPTION 'table % is append-only: % is not allowed', TG_TABLE_NAME, TG_OP;
END;
$$;

CREATE TRIGGER transactions_append_only
    BEFORE UPDATE OR DELETE ON transactions
    FOR EACH ROW EXECUTE FUNCTION forbid_journal_modification();

CREATE TRIGGER transactions_no_truncate
    BEFORE TRUNCATE ON transactions
    FOR EACH STATEMENT EXECUTE FUNCTION forbid_journal_modification();

CREATE TRIGGER postings_append_only
    BEFORE UPDATE OR DELETE ON postings
    FOR EACH ROW EXECUTE FUNCTION forbid_journal_modification();

CREATE TRIGGER postings_no_truncate
    BEFORE TRUNCATE ON postings
    FOR EACH STATEMENT EXECUTE FUNCTION forbid_journal_modification();
