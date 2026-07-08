#!/usr/bin/env bash
set -euo pipefail

ROOT_URL="${ROOT_URL:-http://oif-solver-mainnet-786829271.eu-west-3.elb.amazonaws.com}"
API_URL="${API_URL:-$ROOT_URL/api/v1}"
ORDER_ID="${ORDER_ID:-0x82bb66d9a22779cedc1a1c3937b264d8cc02d175f3165395042313b743181bda}"

echo "== health =="
curl -fsS "$ROOT_URL/health" | jq '{status, solver_id}'

echo
echo "== assets =="
curl -fsS "$API_URL/assets" | jq '{
  networks: (.networks | to_entries | map({
    chain_id: .value.chain_id,
    name: .value.name,
    type: .value.type,
    assets: .value.assets
  }))
}'

echo
echo "== proof order =="
curl -fsS "$API_URL/orders/$ORDER_ID" | jq '{
  id,
  status,
  settlementStandard: .settlement.data.standard,
  fillTransaction
}'
