#!/bin/bash
# Smoke check for the dev stack: PostgreSQL accepts connections and ledger-api is ready behind the proxy.
# Usage: ./scripts/check-dev-stack.sh   (after ./run-dev up -d)
set -euo pipefail
cd "$(dirname "$0")/.."
touch ./.env.local
set -a; source ./.env; source ./.env.local; set +a
API_URL=${LEDGER_API_URL:-http://api.stablecoin-ledger.localhost}

for _ in $(seq 1 30); do
  if docker compose -p stablecoin-ledger -f .docker/docker-compose-dev.yml exec -T postgres \
       pg_isready -U "$POSTGRES_USER" -d "$POSTGRES_DB" >/dev/null 2>&1; then
    pg_ok=1; break
  fi
  sleep 1
done
[ "${pg_ok:-0}" = 1 ] || { echo "FAIL: postgres is not ready"; exit 1; }
echo "OK: postgres ready"

# The first start compiles ledger-api, so allow a few minutes.
for _ in $(seq 1 180); do
  status=$(curl -s -o /dev/null -w '%{http_code}' "$API_URL/health/ready" || true)
  [ "$status" = 200 ] && break
  sleep 2
done
[ "${status:-}" = 200 ] || { echo "FAIL: ledger-api at $API_URL/health/ready returned '${status:-none}'"; exit 1; }
echo "OK: ledger-api ready at $API_URL"
