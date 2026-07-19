# Treasury + Float: same-chain inventory swaps

**Status:** design spec (not yet implemented). Complements [Cross-Chain Rebalancing](./rebalance.md).

## Goal

Let the solver hold the bulk of its value in a single **treasury asset** (WBTC) while
filling intents that pay out in other assets (ETH/STRK/USDC/USDT). Instead of holding
a spread of fill assets, the solver keeps a small **working float** of each fill asset
per chain and **auto-tops-up the float by swapping treasury → fill asset on the same
chain** when the float dips — via a DEX aggregator, **off the fill critical path**.

Explicitly rejected: **per-order JIT swapping** (swap inside the fill path). It adds
swap-confirm latency to every fill (lost races, missed deadlines), pays slippage twice
per order, and breaks OIF's atomic-fill / stranded-funds invariant. Float top-up is
decoupled: fills spend the existing float; swaps replenish it asynchronously.

## Two distinct operations (do not conflate)

| Operation | Trigger | Mechanism | Status |
|---|---|---|---|
| **Treasury rebalance** (WBTC ↔ WBTC across chains) | a chain's WBTC treasury runs low | LayerZero **OFT bridge** (`BridgeInterface`) | built (`layerzero`) |
| **Float top-up** (WBTC → ETH/STRK/USDC/USDT, same chain) | a chain's float dips below band | **DEX swap** (`SwapInterface`, this spec) | not built |

Float top-up is **same-chain** and changes the **token**; treasury rebalance is
**cross-chain** and keeps the token. They are different enough to warrant a separate
abstraction rather than overloading `BridgeInterface` (whose semantics assume a token
moving across chains, `supported_routes` returns chain pairs, `estimate_fee` means a
messaging fee — none of which map cleanly to a same-chain swap).

## Architecture

### `SwapInterface` (new, sibling to `BridgeInterface`)

```
trait SwapInterface {
    // Quote treasury->float on `chain_id`. Pure/read-only, safe on mainnet.
    async fn quote(&self, chain_id, from_token, to_token, amount_in)
        -> Result<SwapQuote>;   // { amount_out, price_impact_bps, route }
    // Execute the swap (approve + swap). Returns the tx hash.
    async fn swap(&self, chain_id, from_token, to_token, amount_in, min_out, scope)
        -> Result<TransactionHash>;
    async fn check_status(&self, chain_id, tx_hash) -> Result<SwapStatus>;
}
```

Backends:
- **`AvnuSwap` (Starknet)** — quote via AVNU API (`/swap/v2/quotes`), build the
  `multi_route_swap` calldata, submit as a Starknet invoke multicall (approve + swap)
  through `DeliveryService::deliver_system_execution` (the same path the OFT send uses).
- **`UniswapSwap` (Ethereum)** — keyless (no aggregator API dependency). Quote via
  Uniswap v3 `QuoterV2.quoteExactInput` (confirmed working: 0.1 WBTC → 6,419.71 USDC on
  the 0.3% pool), execute via `SwapRouter02.exactInput` with an operator-configured
  encoded **path** (multi-hop supported — e.g. `WBTC-0.3%-WETH-0.05%-USDC` where no
  direct pool is best). We pick the path/fee tiers per pair in config rather than letting
  an aggregator optimize splits; acceptable since the Ethereum floats (WETH/USDC/USDT)
  all have deep direct WBTC pools.

Both reuse `DeliveryService`; no new signing path.

### Monitor extension

Add a **float-top-up pass** to `RebalanceMonitor::tick`, after the cross-chain pairs
pass, iterating configured float targets:

```
for each (chain, float_asset) in float_targets:
    bal   = get_balance(chain, solver, float_asset)      // reuse per-chain solver-addr resolution
    if bal >= target*(1 - band):  continue               // within band
    deficit = target - bal
    quote = swap.quote(chain, WBTC, float_asset, wbtc_for(deficit))
    if quote.price_impact_bps > max_slippage_bps: skip+log   // fee/slippage cap (fails closed)
    if treasury_balance(chain) < wbtc_for(deficit):
        # local treasury insufficient -> defer to the WBTC OFT pair to bring treasury,
        # then top up next tick. Do NOT block.
        log; continue
    swap.swap(chain, WBTC, float_asset, amount_in, min_out=quote.amount_out*(1-slip), scope)
```

Reuse the existing threshold/cooldown/pair-lock machinery, the per-chain solver-address
resolution, and the `send_enabled`-style gate (here `swap_enabled`, default false).

## Config

```jsonc
"rebalance": {
  "float": {                                  // new section
    "swap_implementation": "avnu|uniswap",     // avnu on Starknet, uniswap on Ethereum
    "treasury_token": { "1": "0x2260…", "358974494": "0x03fe2b97c…" },   // WBTC per chain
    "max_slippage_bps": 100,                   // reject a swap whose quote impact exceeds this
    "targets": [
      { "chain_id": 1,        "token": "0xA0b8…USDC", "target": "5000000000", "band_bps": 2500 },
      { "chain_id": 358974494,"token": "0x053c…USDC", "target": "5000000000", "band_bps": 2500 }
      // …ETH/STRK/USDT entries
    ],
    "swap_config": {                           // per-backend
      "avnu":    { "exchange_address": "0x…", "quote_api": "https://starknet.api.avnu.fi" },
      "uniswap": { "quoter": "0x61fFE014bA17989E743c5F6cB21bF9697530B21e",
                   "router": "0x…SwapRouter02",
                   "paths": { "USDC": "0x2260…WBTC-0001f4-…USDC", "WETH": "…" } }  // encoded fee-tier paths
    },
    "swap_enabled": false                      // real-funds gate (quote-only until flipped)
  }
}
```

## Slippage / cost guard

Verified mainnet liquidity (2026-07-19): WBTC → {USDC, ETH, STRK} costs **~0.3–0.4% at
~$6k**, ~0.6–0.9% at ~$32k on Starknet (AVNU/Ekubo); Ethereum WBTC↔USDC ~0.48% single-pool
(aggregator lower). So `max_slippage_bps: 100` (1%) is a safe default; floats should be
sized so a top-up is ≤ ~$6–10k (keeps impact <0.4%). Larger top-ups should split.

## Failure semantics & invariants

- **A float top-up never gates a fill.** If a float is empty, the fill fails/skips exactly
  as today — we do not block a fill waiting on a swap. This preserves the fill critical
  path and OIF's atomicity.
- **A failed swap strands nothing** — the solver still holds WBTC. No cross-chain
  in-flight state, no partial-bridge risk. `check_status` + cooldown handle retries.
- **Gas float is mandatory and separate** — ETH (Ethereum) / STRK (Starknet) are needed
  to pay for the swap tx and fills themselves; those floats can never be WBTC. The
  monitor must keep a native-gas reserve (reuse `min_native_gas_reserve`) before swapping.

## Rollout

1. `swap_enabled: false` → quotes run (safe), swaps refuse. Watch the monitor log
   "float low, would swap X→Y (impact Z bps)".
2. An ignored mainnet quote probe per backend (mirror `quote_probe_wbtc_starknet_to_eth`).
3. Flip `swap_enabled: true` per deployment after verifying a manual swap.

## Testing

- Unit: threshold/deficit math; AVNU/aggregator calldata encoders (byte-exact, like
  `starknet_oft.rs`); slippage-cap gate; config parse.
- Ignored integration: live quote probes (no funds).
- Gated execution behind `swap_enabled`.

## Resolved decisions (2026-07-19)

1. **Ethereum swap = Uniswap** (keyless): `QuoterV2` + `SwapRouter02.exactInput` with
   configured paths. No aggregator API-key dependency.
2. **ETH float = WETH** on Ethereum (matches the fill-asset convention WETH↔ETH). If a
   settler ever needs *native* ETH output, unwrap is a follow-up.
3. **Treasury on BOTH chains** — WBTC held on Ethereum and Starknet. So a float top-up
   sources treasury **locally** in the normal case; the WBTC OFT pair only kicks in when
   a chain's local treasury is drained (top-ups faster than the OFT refill). The
   "local insufficient → defer to OFT" branch is the exception, not the norm.
4. **Surplus rides** — a float that runs *high* from fills is left as-is (no swap back to
   WBTC). Revisit only if float drift becomes a capital-efficiency problem.
