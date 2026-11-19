-- Webhook subscriptions (FR-50). Signing needs the secret itself (FR-54), so it is stored
-- encrypted (AES-256-GCM, `nonce || ciphertext`) with a key from the environment, never in clear.
-- After a rotation the previous secret keeps signing until previous_secret_expires_at.
CREATE TABLE webhook_endpoints (
    id                         BIGSERIAL PRIMARY KEY,
    url                        TEXT NOT NULL CHECK (length(url) <= 2048),
    event_types                TEXT[] NOT NULL CHECK (cardinality(event_types) > 0),
    enabled                    BOOLEAN NOT NULL DEFAULT true,
    secret                     BYTEA NOT NULL,
    previous_secret            BYTEA,
    previous_secret_expires_at TIMESTAMPTZ,
    created_at                 TIMESTAMPTZ NOT NULL DEFAULT now(),
    CHECK ((previous_secret IS NULL) = (previous_secret_expires_at IS NULL))
);

-- Creating, changing or rotating a subscription replays as that subscription. No foreign key:
-- a replayed delete must still answer after the row is gone.
ALTER TABLE idempotency_keys ADD COLUMN webhook_id BIGINT;
