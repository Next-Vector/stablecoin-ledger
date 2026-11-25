-- The asset registry (§4.2): which assets the ledger accounts for. Chain details (chain id,
-- contract, finality) belong to the chain scanning module, not to the ledger.
CREATE TABLE assets (
    id         TEXT PRIMARY KEY,
    network    TEXT NOT NULL CHECK (network ~ '^[a-z0-9_]+$'),
    decimals   SMALLINT NOT NULL CHECK (decimals BETWEEN 0 AND 38),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Deposit addresses (FR-20): issued and watched outside the ledger, registered here as plain
-- strings, so any network's address format fits. An address belongs to exactly one account, and
-- an account has one address per network.
CREATE TABLE deposit_addresses (
    id         BIGSERIAL PRIMARY KEY,
    network    TEXT NOT NULL,
    address    TEXT NOT NULL CHECK (length(address) BETWEEN 1 AND 255),
    account    TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT deposit_addresses_address_key UNIQUE (network, address),
    CONSTRAINT deposit_addresses_account_key UNIQUE (network, account)
);

-- A registration replays as that address.
ALTER TABLE idempotency_keys ADD COLUMN deposit_address_id BIGINT REFERENCES deposit_addresses(id);
