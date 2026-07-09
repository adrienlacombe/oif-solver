#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'EOF'
Preflight a live Hyperlane7683 destination route.

Required:
  DESTINATION_RPC_URL     RPC URL for the chain where the fill/settle contract lives
  DESTINATION_SETTLER     Hyperlane7683 destination settler/router address
  ORIGIN_DOMAIN           Hyperlane domain id of the origin chain

Optional:
  DESTINATION_CHAIN_ID    Assert the destination RPC reports this chain id
  ALLOW_ZERO_QUOTE=1      Permit zero quoteGasPayment/quoteDispatch results

Optional full dispatch quote:
  ORDER_ID                Filled order id, bytes32 hex
  FILLER_DATA             Filler data bytes used by fill(), hex bytes
  REFUND_ADDRESS          EVM refund address for standard hook metadata

Example:
  DESTINATION_RPC_URL=https://base-sepolia.example \
  DESTINATION_SETTLER=0x... \
  ORIGIN_DOMAIN=11155420 \
  scripts/flow/preflight-hyperlane7683-route.sh
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

strip_0x() {
  local value="$1"
  echo "${value#0x}"
}

pad64() {
  local value
  value="$(strip_0x "$1")"
  printf "%064s" "$value" | tr ' ' '0'
}

to_dec() {
  local value="$1"
  cast to-dec "$value" 2>/dev/null || echo "$value"
}

call_first_line() {
  local target="$1"
  local signature="$2"
  shift 2

  cast call "$target" "$signature" "$@" --rpc-url "$DESTINATION_RPC_URL" | sed -n '1p'
}

require_code() {
  local label="$1"
  local address="$2"
  local code

  code="$(cast code "$address" --rpc-url "$DESTINATION_RPC_URL")"
  if [[ "$code" == "0x" || -z "$code" ]]; then
    echo "missing code for $label at $address" >&2
    exit 1
  fi
  echo "$label code: ok"
}

require_nonzero_quote() {
  local label="$1"
  local value="$2"
  local dec

  dec="$(to_dec "$value")"
  echo "$label: $dec"
  if [[ "$dec" == "0" && "${ALLOW_ZERO_QUOTE:-0}" != "1" ]]; then
    echo "$label returned zero; set ALLOW_ZERO_QUOTE=1 only for mock/local routes" >&2
    exit 1
  fi
}

metadata_for_standard_hook() {
  local gas_limit="$1"
  local refund_address="$2"
  local refund_hex

  refund_hex="$(strip_0x "$refund_address")"
  if [[ ${#refund_hex} -ne 40 ]]; then
    echo "REFUND_ADDRESS must be a 20-byte EVM address" >&2
    exit 2
  fi

  echo "0x0001$(pad64 0)$(pad64 "$(cast to-hex "$gas_limit")")$refund_hex"
}

main() {
  require_cmd cast
  require_env DESTINATION_RPC_URL
  require_env DESTINATION_SETTLER
  require_env ORIGIN_DOMAIN

  echo "== destination rpc =="
  chain_id="$(cast chain-id --rpc-url "$DESTINATION_RPC_URL")"
  echo "chain_id: $chain_id"
  if [[ -n "${DESTINATION_CHAIN_ID:-}" && "$chain_id" != "$DESTINATION_CHAIN_ID" ]]; then
    echo "destination chain mismatch: expected $DESTINATION_CHAIN_ID, got $chain_id" >&2
    exit 1
  fi

  echo
  echo "== contract code =="
  require_code "destination settler" "$DESTINATION_SETTLER"

  echo
  echo "== hyperlane7683 route =="
  mailbox="$(call_first_line "$DESTINATION_SETTLER" 'mailbox()(address)')"
  hook="$(call_first_line "$DESTINATION_SETTLER" 'hook()(address)')"
  destination_gas="$(call_first_line "$DESTINATION_SETTLER" 'destinationGas(uint32)(uint256)' "$ORIGIN_DOMAIN")"
  router="$(call_first_line "$DESTINATION_SETTLER" 'routers(uint32)(bytes32)' "$ORIGIN_DOMAIN")"
  quote_gas_payment="$(call_first_line "$DESTINATION_SETTLER" 'quoteGasPayment(uint32)(uint256)' "$ORIGIN_DOMAIN")"

  echo "mailbox: $mailbox"
  echo "hook: $hook"
  echo "destinationGas($ORIGIN_DOMAIN): $(to_dec "$destination_gas")"
  echo "router[$ORIGIN_DOMAIN]: $router"
  if [[ "$(strip_0x "$router")" =~ ^0+$ ]]; then
    echo "router enrollment is zero for origin domain $ORIGIN_DOMAIN" >&2
    exit 1
  fi
  require_code "mailbox" "$mailbox"
  require_code "hook" "$hook"
  require_nonzero_quote "quoteGasPayment($ORIGIN_DOMAIN)" "$quote_gas_payment"

  echo
  echo "== full mailbox quote =="
  if [[ -z "${ORDER_ID:-}" || -z "${FILLER_DATA:-}" || -z "${REFUND_ADDRESS:-}" ]]; then
    echo "skipped; set ORDER_ID, FILLER_DATA, and REFUND_ADDRESS after a fill to quote exact settle dispatch"
    exit 0
  fi

  message_body="$(cast abi-encode 'f(bool,bytes32[],bytes[])' true "[$ORDER_ID]" "[$FILLER_DATA]")"
  metadata="$(metadata_for_standard_hook "$(to_dec "$destination_gas")" "$REFUND_ADDRESS")"
  quote_dispatch="$(
    call_first_line \
      "$mailbox" \
      'quoteDispatch(uint32,bytes32,bytes,bytes,address)(uint256)' \
      "$ORIGIN_DOMAIN" \
      "$router" \
      "$message_body" \
      "$metadata" \
      "$hook"
  )"
  require_nonzero_quote "Mailbox.quoteDispatch($ORIGIN_DOMAIN)" "$quote_dispatch"
}

main "$@"
