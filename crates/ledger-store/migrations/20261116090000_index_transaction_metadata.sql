-- History filter by a metadata key/value pair (FR-06) uses `metadata @> {...}`.
CREATE INDEX transactions_metadata_idx ON transactions USING GIN (metadata jsonb_path_ops);
