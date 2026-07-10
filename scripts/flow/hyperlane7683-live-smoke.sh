#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'EOF'
Run a self-contained live/testnet Hyperlane7683 solver smoke against file storage.

This script seeds its own file-backed solver config from a template (live RPC +
account settings come from the environment), runs solver balance preflight
checks, sets a Starknet fee cap, starts the solver, opens or watches an order,
and verifies the EVM claim receipt.

No Redis is required: the solver runs with STORAGE_BACKEND=file and the admin API
initializes against the same file backend.

Required environment (RPC + account):
  ETHEREUM_RPC_URL                 Ethereum Sepolia RPC (origin/claim chain)
  STARKNET_RPC_URL                 Starknet Sepolia RPC (fill chain)
  SOLVER_PRIVATE_KEY               EVM solver signer key (never written to disk)
  SOLVER_ADDRESS                   EVM solver address (balance preflight)
  SOLVER_STARKNET_PRIVATE_KEY      Starknet solver signer key (never written to disk)
  SOLVER_STARKNET_ACCOUNT_ADDRESS  Starknet solver account address

One of (how to obtain the order to watch):
  ORDER_ID                         Existing order id to watch (bytes32 hex)
  HYPERLANE7683_SUBMIT_CMD         Command that opens/submits a fresh order

Optional environment (with defaults):
  SMOKE_SOLVER_ID                  Solver id; default: oif-hyperlane7683-sepolia
  MAX_STARKNET_FEE_FRI             Starknet fee cap, fri; default: 25000000000000000000
                                   (25 STRK, above the ~17.12 STRK open estimate)
  MIN_SOLVER_STRK_FRI              Min solver STRK balance, fri; default: MAX_STARKNET_FEE_FRI
  MIN_SOLVER_ETH_WEI               Min solver ETH balance, wei; default: 20000000000000000 (0.02 ETH)
  SKIP_BALANCE_CHECKS              Set to 1 to skip balance preflight; default: 0
  SMOKE_CONFIG_TEMPLATE            Config template; default: config/hyperlane7683-sepolia-smoke.json
  STORAGE_PATH                     File storage root; default: a fresh temp dir
  FORCE_SEED                       Set to 1 to re-seed an existing STORAGE_PATH; default: 0
  JWT_SECRET                       Admin API JWT secret (admin is enabled but unused here)
  HYPERLANE7683_SMOKE_ENV_FILE     Env file to source before running
  SOLVER_CMD                       Solver command; default: ./target/debug/solver --log-level info
  SOLVER_API_URL                   Health URL base; default: http://127.0.0.1:3000
  SMOKE_TIMEOUT_SECONDS            Finalization timeout; default: 900
  SMOKE_POLL_SECONDS               Poll interval; default: 5
  START_SOLVER                     Start/stop solver in this script; default: 1
  SOLVER_INGRESS_MODE              Passed through to the solver if set
  CLAIM_RPC_URL                    Receipt RPC; default: ETHEREUM_RPC_URL

The script intentionally avoids printing private keys or full environment.
Account keys stay in the environment and are expanded by the solver at runtime;
they are never written into the seeded config file.
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

# Renders the config template into RUNTIME_CONFIG, substituting only the public
# placeholders (solver id + RPC URLs). Account placeholders are left intact so
# the solver expands them from the environment at runtime; keys never hit disk.
render_config() {
  RUNTIME_CONFIG="$(mktemp "${TMPDIR:-/tmp}/oif-hyperlane7683-smoke-config.XXXXXX.json")"
  chmod 600 "$RUNTIME_CONFIG"
  python3 - "$SMOKE_CONFIG_TEMPLATE" "$RUNTIME_CONFIG" <<'PY'
import json
import os
import re
import sys

template_path, out_path = sys.argv[1], sys.argv[2]
with open(template_path) as handle:
    text = handle.read()

# Public, non-secret placeholders the script resolves itself.
public_vars = ["SMOKE_SOLVER_ID", "ETHEREUM_RPC_URL", "STARKNET_RPC_URL"]
for name in public_vars:
    value = os.environ.get(name)
    if value is None:
        sys.stderr.write(f"missing env for template: {name}\n")
        sys.exit(2)
    text = text.replace("${" + name + "}", value)

# Account placeholders that the solver expands from the environment at runtime.
account_allowed = {
    "SOLVER_PRIVATE_KEY",
    "SOLVER_STARKNET_PRIVATE_KEY",
    "SOLVER_STARKNET_ACCOUNT_ADDRESS",
}
leftovers = set(re.findall(r"\$\{([A-Z0-9_]+)\}", text))
unexpected = leftovers - account_allowed
if unexpected:
    sys.stderr.write(
        "unexpected unresolved placeholders in config template: "
        + ", ".join(sorted(unexpected))
        + "\n"
    )
    sys.exit(2)

# Fail fast on malformed config before handing it to the solver.
json.loads(text)

with open(out_path, "w") as handle:
    handle.write(text)
PY
  echo "rendered solver config: $RUNTIME_CONFIG"
}

check_evm_balance() {
  local balance
  balance="$(cast balance "$SOLVER_ADDRESS" --rpc-url "$ETHEREUM_RPC_URL")"
  if ! python3 - "$balance" "$MIN_SOLVER_ETH_WEI" "$SOLVER_ADDRESS" <<'PY'
import sys

balance = int(sys.argv[1])
minimum = int(sys.argv[2])
address = sys.argv[3]
print(f"solver EVM balance: {balance} wei (min {minimum})")
if balance < minimum:
    sys.stderr.write(f"solver {address} EVM balance {balance} wei is below minimum {minimum} wei\n")
    sys.exit(1)
PY
  then
    return 1
  fi
}

# Reads the Starknet fee-token (STRK) balance of the solver account, in fri.
# Best-effort: requires starkli (which computes the entrypoint selector safely).
check_starknet_balance() {
  if ! command -v starkli >/dev/null 2>&1; then
    echo "WARNING: starkli not found; skipping Starknet STRK balance preflight" >&2
    echo "         install starkli or set SKIP_BALANCE_CHECKS=1 to silence this" >&2
    return 0
  fi

  local strk_token
  strk_token="$(jq -r '.settlement.hyperlane.starknet_fee_token_addresses["23448591"]' "$RUNTIME_CONFIG")"
  if [[ -z "$strk_token" || "$strk_token" == "null" ]]; then
    echo "WARNING: no Starknet fee token in config; skipping STRK balance preflight" >&2
    return 0
  fi

  local raw
  # OpenZeppelin Cairo ERC20 exposes both camelCase and snake_case; try both.
  if ! raw="$(starkli call "$strk_token" balanceOf "$SOLVER_STARKNET_ACCOUNT_ADDRESS" \
      --rpc-url "$STARKNET_RPC_URL" 2>/dev/null)"; then
    raw="$(starkli call "$strk_token" balance_of "$SOLVER_STARKNET_ACCOUNT_ADDRESS" \
      --rpc-url "$STARKNET_RPC_URL")"
  fi

  if ! python3 - "$MIN_SOLVER_STRK_FRI" "$SOLVER_STARKNET_ACCOUNT_ADDRESS" "$raw" <<'PY'
import sys

minimum = int(sys.argv[1])
address = sys.argv[2]
felts = [line.strip() for line in sys.argv[3].splitlines() if line.strip()]
if len(felts) < 2:
    sys.stderr.write("unexpected starkli balanceOf output; skipping check\n")
    sys.exit(0)
low, high = int(felts[0], 16), int(felts[1], 16)
balance = low + (high << 128)
print(f"solver Starknet STRK balance: {balance} fri (min {minimum})")
if balance < minimum:
    sys.stderr.write(f"solver {address} STRK balance {balance} fri is below minimum {minimum} fri\n")
    sys.exit(1)
PY
  then
    return 1
  fi
}

preflight_balances() {
  if [[ "${SKIP_BALANCE_CHECKS:-0}" == "1" ]]; then
    echo "skipping balance preflight (SKIP_BALANCE_CHECKS=1)"
    return
  fi
  echo "== balance preflight =="
  check_evm_balance
  check_starknet_balance
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
  if [[ -n "${SOLVER_LOG_FILE:-}" && -f "$SOLVER_LOG_FILE" ]]; then
    echo "last solver log lines:" >&2
    tail -n 80 "$SOLVER_LOG_FILE" >&2
  fi
  exit 1
}

start_solver() {
  if [[ "${START_SOLVER:-1}" != "1" ]]; then
    return
  fi

  export STORAGE_BACKEND=file
  export STORAGE_PATH
  export MAX_STARKNET_FEE_FRI
  # The solver expands these account placeholders from its own environment at
  # runtime, so they must be exported to the child process. Keys stay in the
  # environment and are never written to the seeded config on disk.
  export SOLVER_PRIVATE_KEY SOLVER_STARKNET_PRIVATE_KEY SOLVER_STARKNET_ACCOUNT_ADDRESS
  if [[ -n "${SOLVER_INGRESS_MODE:-}" ]]; then
    export SOLVER_INGRESS_MODE
  fi
  export RUST_LOG="${RUST_LOG:-solver_core::recovery=info,solver_core::handlers::settlement=info,solver_core::handlers::transaction=info,solver_core::engine=info,solver_delivery=info,solver_settlement=info,solver_service=info,info}"

  local log_dir="${SMOKE_LOG_DIR:-${TMPDIR:-/tmp}/oif-hyperlane7683-smoke-$(date +%s)}"
  mkdir -p "$log_dir"
  SOLVER_LOG_FILE="$log_dir/solver.log"
  export SOLVER_LOG_FILE

  local base_cmd="${SOLVER_CMD:-./target/debug/solver --log-level info}"
  local solver_cmd="$base_cmd --bootstrap-config $RUNTIME_CONFIG"
  if [[ "${FORCE_SEED:-0}" == "1" ]]; then
    solver_cmd="$solver_cmd --force-seed"
  fi
  echo "starting solver (STORAGE_BACKEND=file, MAX_STARKNET_FEE_FRI=$MAX_STARKNET_FEE_FRI)"
  echo "solver log: $SOLVER_LOG_FILE"
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
  if [[ -n "${RUNTIME_CONFIG:-}" && -f "$RUNTIME_CONFIG" ]]; then
    rm -f "$RUNTIME_CONFIG"
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

  # Compatibility with the adrien-oif-starknet solver .env variable naming.
  # Canonical names win if already set; otherwise fall back to that project's.
  SOLVER_ADDRESS="${SOLVER_ADDRESS:-${SOLVER_PUB_KEY:-}}"
  SOLVER_STARKNET_PRIVATE_KEY="${SOLVER_STARKNET_PRIVATE_KEY:-${STARKNET_SOLVER_PRIVATE_KEY:-}}"
  SOLVER_STARKNET_ACCOUNT_ADDRESS="${SOLVER_STARKNET_ACCOUNT_ADDRESS:-${STARKNET_SOLVER_ADDRESS:-}}"

  require_cmd jq
  require_cmd dd
  require_cmd cast
  require_cmd curl
  require_cmd python3

  require_env ETHEREUM_RPC_URL
  require_env STARKNET_RPC_URL
  require_env SOLVER_PRIVATE_KEY
  require_env SOLVER_ADDRESS
  require_env SOLVER_STARKNET_PRIVATE_KEY
  require_env SOLVER_STARKNET_ACCOUNT_ADDRESS

  if [[ -z "${ORDER_ID:-}" && -z "${HYPERLANE7683_SUBMIT_CMD:-}" ]]; then
    echo "ORDER_ID or HYPERLANE7683_SUBMIT_CMD is required" >&2
    echo >&2
    usage >&2
    exit 2
  fi

  # Canonical solver id shared by the seeded config and the storage layout.
  SMOKE_SOLVER_ID="${SMOKE_SOLVER_ID:-${SOLVER_ID:-oif-hyperlane7683-sepolia}}"
  SOLVER_ID="$SMOKE_SOLVER_ID"
  export SMOKE_SOLVER_ID SOLVER_ID

  # Fee cap must sit above the current ~17.12 STRK open estimate. Values use
  # 18-decimal fri and exceed 64-bit range, so compare with python, not bash.
  STARKNET_FEE_FLOOR_FRI="${STARKNET_FEE_FLOOR_FRI:-17120000000000000000}"
  local default_fee_cap=25000000000000000000
  MAX_STARKNET_FEE_FRI="${MAX_STARKNET_FEE_FRI:-$default_fee_cap}"
  # If a sourced env pinned the cap at or below the floor, raise it so the smoke
  # is not doomed by the very shortfall this run is meant to prove is fixed.
  if python3 -c "import sys; sys.exit(0 if int('$MAX_STARKNET_FEE_FRI') <= int('$STARKNET_FEE_FLOOR_FRI') else 1)"; then
    echo "WARNING: MAX_STARKNET_FEE_FRI=$MAX_STARKNET_FEE_FRI is at/below the ${STARKNET_FEE_FLOOR_FRI} fri floor; raising to $default_fee_cap" >&2
    MAX_STARKNET_FEE_FRI="$default_fee_cap"
  fi
  MIN_SOLVER_STRK_FRI="${MIN_SOLVER_STRK_FRI:-$MAX_STARKNET_FEE_FRI}"
  MIN_SOLVER_ETH_WEI="${MIN_SOLVER_ETH_WEI:-20000000000000000}"
  export MAX_STARKNET_FEE_FRI

  SMOKE_CONFIG_TEMPLATE="${SMOKE_CONFIG_TEMPLATE:-config/hyperlane7683-sepolia-smoke.json}"
  if [[ ! -r "$SMOKE_CONFIG_TEMPLATE" ]]; then
    echo "config template not readable: $SMOKE_CONFIG_TEMPLATE" >&2
    exit 2
  fi

  # Admin API is enabled in the template; provide a test-only JWT secret if the
  # caller did not. It is not used by this smoke (no admin calls are made).
  export JWT_SECRET="${JWT_SECRET:-smoke-test-only-jwt-secret-not-for-production-use}"

  # Fresh, isolated file storage unless the caller pins STORAGE_PATH.
  if [[ -z "${STORAGE_PATH:-}" ]]; then
    STORAGE_PATH="$(mktemp -d "${TMPDIR:-/tmp}/oif-hyperlane7683-smoke-storage.XXXXXX")"
  fi
  mkdir -p "$STORAGE_PATH"
  export STORAGE_PATH
  echo "storage path: $STORAGE_PATH (backend=file)"

  render_config
  preflight_balances
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
