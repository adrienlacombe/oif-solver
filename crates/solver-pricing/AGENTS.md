# solver-pricing - Agent Guide

Pricing oracle abstraction and provider implementations.

## What lives here

- `PricingInterface`, `PricingService`, provider registries, and fallback sequencing.
- Mock pricing for tests/dev plus live providers such as CoinGecko and DefiLlama.
- Asset conversion and fee/pricing config used by execution strategy code.

Order profitability decisions belong in `solver-order` strategies; gas estimation and transaction fees belong in `solver-delivery` and settlement backends.

## Numeric handling

Use decimal/string-based conversions for token and currency amounts. Avoid floating-point math for externally visible pricing or amounts.

## External providers

Keep provider failures explicit enough for fallback behavior to work. Tests for live-provider adapters should mock HTTP with `wiremock`; do not depend on real APIs in default tests.

## Verification

```
cargo test -p solver-pricing
cargo check -p solver-pricing
```
