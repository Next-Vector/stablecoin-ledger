#!/bin/bash
# Runs the integration tests against the run-integration stack (start it first: ./run-integration up -d --wait).
# Tests share one database and reset its schema, so they run on a single thread.
set -euo pipefail
cd "$(dirname "$0")/.."
touch ./.env.integration.local
set -a; source ./.env.integration; source ./.env.integration.local; set +a
cargo test --workspace --features ledger-store/integration,ledger-api/integration --locked -- --test-threads=1 "$@"
