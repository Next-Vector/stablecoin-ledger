#!/bin/bash
# Benchmarks transfer throughput and latency (NFR-04, NFR-05) of a release ledger-api.
# Usage: BENCH_DATABASE_URL=postgres://… ./scripts/bench.sh [ledger-bench options]
#   The database is reset (migrated, key issued); point it at a throwaway one, e.g. the `bench`
#   database of the stablecoin-dev PostgreSQL. ledger-bench options default to 10,000 accounts,
#   64 in flight, 30 s; add `--rate N` for the latency run.
set -euo pipefail
cd "$(dirname "$0")/.."
: "${BENCH_DATABASE_URL:?set BENCH_DATABASE_URL to a throwaway PostgreSQL database}"

cargo build --release --locked -q -p ledger-api -p ledger-bench
export DATABASE_URL=$BENCH_DATABASE_URL
export BIND_ADDR=${BENCH_BIND_ADDR:-127.0.0.1:8090}
export WEBHOOK_SECRET_KEY=${WEBHOOK_SECRET_KEY:-$(printf '0%.0s' {1..64})}
export DATABASE_MAX_CONNECTIONS=${DATABASE_MAX_CONNECTIONS:-32}
export RUST_LOG=${RUST_LOG:-warn}

./target/release/ledger-api migrate >/dev/null
key=$(./target/release/ledger-api create-api-key --role write --name bench | grep -o 'sl_[A-Za-z0-9_]*' | head -1)

./target/release/ledger-api serve >/dev/null 2>&1 &
server=$!
trap 'kill $server 2>/dev/null; wait $server 2>/dev/null' EXIT
for _ in $(seq 1 50); do
  curl -sf -o /dev/null "http://$BIND_ADDR/health/ready" && break
  sleep 0.2
done

echo "pool: $DATABASE_MAX_CONNECTIONS connections; $(nproc) CPUs"
./target/release/ledger-bench --url "http://$BIND_ADDR" --key "$key" "$@"
