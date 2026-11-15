#!/bin/bash
# Smoke check for the dev stack: PostgreSQL accepts connections and anvil answers JSON-RPC through the proxy.
# Usage: ./scripts/check-dev-stack.sh   (after ./run-dev up -d)
set -euo pipefail
cd "$(dirname "$0")/.."
touch ./.env.local
set -a; source ./.env; source ./.env.local; set +a

for _ in $(seq 1 30); do
  if docker compose -p stablecoin-ledger -f .docker/docker-compose-dev.yml exec -T postgres \
       pg_isready -U "$POSTGRES_USER" -d "$POSTGRES_DB" >/dev/null 2>&1; then
    pg_ok=1; break
  fi
  sleep 1
done
[ "${pg_ok:-0}" = 1 ] || { echo "FAIL: postgres is not ready"; exit 1; }
echo "OK: postgres ready"

for _ in $(seq 1 30); do
  chain_id=$(curl -sf -X POST -H 'Content-Type: application/json' \
    --data '{"jsonrpc":"2.0","id":1,"method":"eth_chainId","params":[]}' "$ETH_RPC_URL" \
    | sed -n 's/.*"result":"\(0x[0-9a-f]*\)".*/\1/p') && [ -n "$chain_id" ] && break
  sleep 1
done
[ "$((chain_id))" = "$ANVIL_CHAIN_ID" ] || { echo "FAIL: anvil at $ETH_RPC_URL returned chain id '${chain_id:-none}', expected $ANVIL_CHAIN_ID"; exit 1; }
echo "OK: anvil chain id $ANVIL_CHAIN_ID at $ETH_RPC_URL"
