#!/usr/bin/env bash
set -euo pipefail

LIFI_ORDER_URL="${LIFI_ORDER_URL:-https://order.li.fi}"
DRY_RUN="${DRY_RUN:-1}"

: "${LIFI_SOLVER_API_KEY:?set LIFI_SOLVER_API_KEY from the LI.FI solver UI}"
: "${EXCLUSIVE_FOR:?set EXCLUSIVE_FOR to the registered solver fill address}"

FROM_CHAIN_ID="${FROM_CHAIN_ID:-358974494}"
TO_CHAIN_ID="${TO_CHAIN_ID:-1}"
FROM_ASSET="${FROM_ASSET:-0x049d36570d4e46f48e99674bd3fcc84644ddd6b96f7c741b1562b82f9e004dc7}"
TO_ASSET="${TO_ASSET:-0xca14007eff0db1f8135f4c25b34de49ab0d42766}"
FROM_DECIMALS="${FROM_DECIMALS:-18}"
TO_DECIMALS="${TO_DECIMALS:-18}"
MIN_AMOUNT="${MIN_AMOUNT:-0.01}"
MAX_AMOUNT="${MAX_AMOUNT:-0.05}"
QUOTE_RATE="${QUOTE_RATE:-1.0}"
MAX_TO_AMOUNT="${MAX_TO_AMOUNT:-}"
EXPIRY_SECONDS="${EXPIRY_SECONDS:-3600}"

EXPIRY="$(($(date +%s) + EXPIRY_SECONDS))"

payload="$(
  jq -n \
    --arg expiry "$EXPIRY" \
    --arg fromChainId "$FROM_CHAIN_ID" \
    --arg toChainId "$TO_CHAIN_ID" \
    --arg fromAsset "$FROM_ASSET" \
    --arg toAsset "$TO_ASSET" \
    --arg fromDecimals "$FROM_DECIMALS" \
    --arg toDecimals "$TO_DECIMALS" \
    --arg minAmount "$MIN_AMOUNT" \
    --arg maxAmount "$MAX_AMOUNT" \
    --arg quote "$QUOTE_RATE" \
    --arg exclusiveFor "$EXCLUSIVE_FOR" \
    --arg maxToAmount "$MAX_TO_AMOUNT" \
    '{
      quotes: [
        {
          expiry: ($expiry | tonumber),
          fromChainId: $fromChainId,
          fromAsset: $fromAsset,
          fromDecimals: ($fromDecimals | tonumber),
          toChainId: $toChainId,
          toAsset: $toAsset,
          toDecimals: ($toDecimals | tonumber),
          ranges: [
            {
              minAmount: $minAmount,
              maxAmount: $maxAmount,
              quote: $quote
            }
          ],
          exclusiveFor: $exclusiveFor
        }
      ]
    }
    | if $maxToAmount == "" then . else .quotes[0].maxToAmount = $maxToAmount end'
)"

if [[ "$DRY_RUN" == "1" ]]; then
  echo "$payload" | jq .
  echo
  echo "DRY_RUN=1; set DRY_RUN=0 to submit to $LIFI_ORDER_URL/quotes/submit"
  exit 0
fi

curl -fsS -X POST "$LIFI_ORDER_URL/quotes/submit" \
  -H 'Content-Type: application/json' \
  -H "api-key: $LIFI_SOLVER_API_KEY" \
  -d "$payload" | jq .
