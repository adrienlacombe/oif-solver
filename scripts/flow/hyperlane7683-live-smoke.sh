#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'EOF'
Run a live/testnet Hyperlane7683 solver smoke against file storage.

The script can either:
  1. recover a known order snapshot:
       ORDER_ID=0x... RESTORE_STORAGE_FROM=/tmp/snapshot scripts/flow/hyperlane7683-live-smoke.sh
  2. submit a fresh order via a caller-provided command:
       HYPERLANE7683_SUBMIT_CMD='cargo run -p solver-demo -- intent submit ... --onchain' \
       scripts/flow/hyperlane7683-live-smoke.sh

Required environment:
  STORAGE_PATH               File storage root used by solver-service
  SOLVER_ID                  Solver id whose config/orders live in STORAGE_PATH
  ETHEREUM_RPC_URL           RPC used to verify the EVM claim receipt

Optional environment:
  HYPERLANE7683_SMOKE_ENV_FILE     Env file to source before running
  ORDER_ID                         Existing order id to watch
  RESTORE_STORAGE_FROM             Snapshot root to copy into STORAGE_PATH first
  HYPERLANE7683_SUBMIT_CMD         Command that opens/submits a fresh order
  SOLVER_CMD                       Solver command; default: ./target/debug/solver --log-level info
  SOLVER_API_URL                   Health URL base; default: http://127.0.0.1:3000
  SMOKE_TIMEOUT_SECONDS            Finalization timeout; default: 900
  SMOKE_POLL_SECONDS               Poll interval; default: 5
  START_SOLVER                     Start/stop solver in this script; default: 1
  SOLVER_INGRESS_MODE              Default: intake_disabled when ORDER_ID is set, otherwise unset
  CLAIM_RPC_URL                    Receipt RPC; default: ETHEREUM_RPC_URL

The script intentionally avoids printing private keys or full environment.
EOF
}

require_cmd() {
  if ! command -v "$1" >/dev/null 2>&1; then
    echo "missing required command: $1" >&2
    exit 127
  fi
}

require_env() {
  if [[ -z "${!1:-}" ]]; then
    echo "missing required env: $1" >&2
    echo >&2
    usage >&2
    exit 2
  fi
}

load_env_file() {
  local env_file="${HYPERLANE7683_SMOKE_ENV_FILE:-}"
  if [[ -z "$env_file" && -f .env ]]; then
    env_file=.env
  fi
  if [[ -n "$env_file" ]]; then
    if [[ ! -r "$env_file" ]]; then
      echo "env file is not readable: $env_file" >&2
      exit 2
    fi
    set -a
    # shellcheck disable=SC1090
    . "$env_file"
    set +a
  fi
}

storage_solver_dir() {
  printf '%s/%s' "${STORAGE_PATH%/}" "$SOLVER_ID"
}

order_file_for() {
  local order_id="$1"
  printf '%s/orders_%s.bin' "$(storage_solver_dir)" "$order_id"
}

list_order_files() {
  find "$(storage_solver_dir)" -maxdepth 1 -type f -name 'orders_0x*.bin' 2>/dev/null | sort
}

restore_storage_snapshot() {
  if [[ -z "${RESTORE_STORAGE_FROM:-}" ]]; then
    return
  fi
  if [[ ! -d "$RESTORE_STORAGE_FROM" ]]; then
    echo "RESTORE_STORAGE_FROM is not a directory: $RESTORE_STORAGE_FROM" >&2
    exit 2
  fi
  mkdir -p "$STORAGE_PATH"
  cp -R "$RESTORE_STORAGE_FROM"/. "$STORAGE_PATH"/
  echo "restored storage snapshot from $RESTORE_STORAGE_FROM"
}

wait_for_health() {
  local base="${SOLVER_API_URL:-http://127.0.0.1:3000}"
  local deadline=$((SECONDS + 60))
  while (( SECONDS < deadline )); do
    if curl -fsS --max-time 2 "$base/health" >/dev/null 2>&1; then
      echo "solver health ok"
      return
    fi
    sleep 1
  done
  echo "solver health did not become ready at $base/health" >&2
  exit 1
}

start_solver() {
  if [[ "${START_SOLVER:-1}" != "1" ]]; then
    return
  fi

  export STORAGE_BACKEND=file
  if [[ -n "${ORDER_ID:-}" ]]; then
    export SOLVER_INGRESS_MODE="${SOLVER_INGRESS_MODE:-intake_disabled}"
  elif [[ -n "${SOLVER_INGRESS_MODE:-}" ]]; then
    export SOLVER_INGRESS_MODE
  fi
  export RUST_LOG="${RUST_LOG:-solver_core::recovery=info,solver_core::handlers::settlement=info,solver_core::handlers::transaction=info,solver_core::engine=info,solver_delivery=info,solver_service=info,info}"

  local log_dir="${SMOKE_LOG_DIR:-${TMPDIR:-/tmp}/oif-hyperlane7683-smoke-$(date +%s)}"
  mkdir -p "$log_dir"
  SOLVER_LOG_FILE="$log_dir/solver.log"
  export SOLVER_LOG_FILE

  local solver_cmd="${SOLVER_CMD:-./target/debug/solver --log-level info}"
  echo "starting solver: $solver_cmd"
  bash -lc "$solver_cmd" >"$SOLVER_LOG_FILE" 2>&1 &
  SOLVER_PID=$!
  export SOLVER_PID
  trap cleanup EXIT
  wait_for_health
}

cleanup() {
  if [[ -n "${SOLVER_PID:-}" ]] && kill -0 "$SOLVER_PID" >/dev/null 2>&1; then
    kill "$SOLVER_PID" >/dev/null 2>&1 || true
    wait "$SOLVER_PID" >/dev/null 2>&1 || true
  fi
}

detect_new_order_after_submit() {
  local before_file
  before_file="$(mktemp)"
  list_order_files >"$before_file"

  echo "running submit command" >&2
  bash -lc "$HYPERLANE7683_SUBMIT_CMD" >&2

  local deadline=$((SECONDS + 120))
  while (( SECONDS < deadline )); do
    while IFS= read -r file; do
      if ! grep -qxF "$file" "$before_file"; then
        basename "$file" | sed -E 's/^orders_(0x[0-9a-fA-F]{64})\.bin$/\1/'
        rm -f "$before_file"
        return
      fi
    done < <(list_order_files)
    sleep 2
  done

  rm -f "$before_file"
  echo "timed out waiting for a new order file after submit command" >&2
  exit 1
}

decode_order_json() {
  local order_file="$1"
  dd if="$order_file" bs=1 skip=64 status=none
}

json_hash_to_hex() {
  python3 - "$1" <<'PY'
import json
import sys

value = json.loads(sys.argv[1])
if value is None:
    sys.exit(0)
print("0x" + "".join(f"{int(byte):02x}" for byte in value))
PY
}

wait_for_finalized_order() {
  local order_id="$1"
  local order_file
  order_file="$(order_file_for "$order_id")"
  local deadline=$((SECONDS + ${SMOKE_TIMEOUT_SECONDS:-900}))
  local poll="${SMOKE_POLL_SECONDS:-5}"

  while (( SECONDS < deadline )); do
    if [[ -f "$order_file" ]]; then
      local order_json status
      order_json="$(decode_order_json "$order_file")"
      status="$(jq -r '.status | if type == "string" then . else @json end' <<<"$order_json")"
      if [[ "$status" == "finalized" ]]; then
        echo "$order_json"
        return
      fi
      echo "order $order_id status: $status" >&2
    else
      echo "waiting for order file $order_file" >&2
    fi
    sleep "$poll"
  done

  echo "timed out waiting for order $order_id to finalize" >&2
  if [[ -n "${SOLVER_LOG_FILE:-}" && -f "$SOLVER_LOG_FILE" ]]; then
    echo "last solver log lines:" >&2
    tail -n 80 "$SOLVER_LOG_FILE" >&2
  fi
  exit 1
}

verify_claim_receipt() {
  local order_json="$1"
  local claim_json tx_hash status block_number
  claim_json="$(jq -c '.claim_tx_hash // (.claim_tx_hashes[0] // null)' <<<"$order_json")"
  tx_hash="$(json_hash_to_hex "$claim_json")"
  if [[ -z "$tx_hash" ]]; then
    echo "finalized order has no claim tx hash" >&2
    exit 1
  fi

  local rpc_url="${CLAIM_RPC_URL:-${ETHEREUM_RPC_URL:-}}"
  if [[ -z "$rpc_url" ]]; then
    echo "CLAIM_RPC_URL or ETHEREUM_RPC_URL is required to verify the claim receipt" >&2
    exit 2
  fi

  local receipt
  receipt="$(cast receipt --rpc-url "$rpc_url" "$tx_hash" --json)"
  status="$(jq -r '.status' <<<"$receipt")"
  block_number="$(jq -r '.blockNumber' <<<"$receipt")"
  if [[ "$status" != "0x1" ]]; then
    echo "claim tx $tx_hash receipt status is $status" >&2
    exit 1
  fi
  echo "claim receipt ok: tx=$tx_hash block=$block_number"
}

main() {
  if [[ "${1:-}" == "-h" || "${1:-}" == "--help" ]]; then
    usage
    exit 0
  fi

  load_env_file
  require_cmd jq
  require_cmd dd
  require_cmd cast
  require_cmd curl
  require_env STORAGE_PATH
  require_env SOLVER_ID

  if [[ -z "${ORDER_ID:-}" && -z "${HYPERLANE7683_SUBMIT_CMD:-}" ]]; then
    echo "ORDER_ID or HYPERLANE7683_SUBMIT_CMD is required" >&2
    echo >&2
    usage >&2
    exit 2
  fi

  restore_storage_snapshot
  if [[ ! -d "$(storage_solver_dir)" ]]; then
    echo "storage solver directory does not exist: $(storage_solver_dir)" >&2
    exit 2
  fi

  start_solver

  local order_id="${ORDER_ID:-}"
  if [[ -z "$order_id" ]]; then
    order_id="$(detect_new_order_after_submit)"
    echo "detected order: $order_id"
  else
    echo "watching order: $order_id"
  fi

  local order_json
  order_json="$(wait_for_finalized_order "$order_id")"
  verify_claim_receipt "$order_json"
  echo "hyperlane7683 smoke passed: order=$order_id"
}

main "$@"
