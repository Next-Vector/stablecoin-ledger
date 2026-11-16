-- Transactional outbox (FR-52): each event is written in the same DB transaction as the change
-- it describes, so it is never lost and never appears without the change. `sequence` orders the
-- events of one resource (FR-55); `id` is what receivers deduplicate on.
CREATE TABLE webhook_events (
    id         TEXT PRIMARY KEY CHECK (id ~ '^evt_[0-9a-f]{32}$'),
    type       TEXT NOT NULL,
    resource   TEXT NOT NULL,
    sequence   BIGINT NOT NULL CHECK (sequence > 0),
    data       JSONB NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (resource, sequence)
);

CREATE INDEX webhook_events_created_at_idx ON webhook_events (created_at);
