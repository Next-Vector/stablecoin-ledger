-- Manual redelivery (FR-56) queues a fresh delivery of an event next to the finished ones, so a
-- delivery gets its own id; at most one delivery per event and subscription is pending at a time.
ALTER TABLE webhook_deliveries DROP CONSTRAINT webhook_deliveries_pkey;
ALTER TABLE webhook_deliveries ADD COLUMN id BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY;
CREATE UNIQUE INDEX webhook_deliveries_one_pending_idx ON webhook_deliveries (event_id, endpoint_id)
    WHERE status = 'pending';
CREATE INDEX webhook_deliveries_event_idx ON webhook_deliveries (event_id);

-- The log of delivery attempts (FR-56): response code, latency and error of each one. endpoint_id
-- is copied from the delivery so the log of one subscription is read by index.
CREATE TABLE webhook_delivery_attempts (
    id           BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    delivery_id  BIGINT NOT NULL REFERENCES webhook_deliveries (id) ON DELETE CASCADE,
    endpoint_id  BIGINT NOT NULL REFERENCES webhook_endpoints (id) ON DELETE CASCADE,
    attempt      INTEGER NOT NULL CHECK (attempt >= 1),
    succeeded    BOOLEAN NOT NULL,
    status_code  INTEGER,
    latency_ms   BIGINT NOT NULL CHECK (latency_ms >= 0),
    error        TEXT,
    attempted_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (delivery_id, attempt)
);

CREATE INDEX webhook_delivery_attempts_endpoint_idx ON webhook_delivery_attempts (endpoint_id, id DESC);
