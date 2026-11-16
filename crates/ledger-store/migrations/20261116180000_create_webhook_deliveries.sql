-- One delivery per event and subscription (FR-53), fanned out in the same DB transaction as the
-- event to the subscriptions enabled for its type at that moment. A dispatcher claims a due row
-- by pushing next_attempt_at forward (a lease), so a delivery lost with a crashed process is
-- retried once the lease runs out (at-least-once).
CREATE TABLE webhook_deliveries (
    event_id         TEXT NOT NULL REFERENCES webhook_events (id),
    endpoint_id      BIGINT NOT NULL REFERENCES webhook_endpoints (id) ON DELETE CASCADE,
    status           TEXT NOT NULL DEFAULT 'pending' CHECK (status IN ('pending', 'succeeded', 'failed')),
    attempts         INTEGER NOT NULL DEFAULT 0 CHECK (attempts >= 0),
    next_attempt_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_status_code INTEGER,
    last_error       TEXT,
    updated_at       TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (event_id, endpoint_id)
);

CREATE INDEX webhook_deliveries_due_idx ON webhook_deliveries (next_attempt_at) WHERE status = 'pending';
