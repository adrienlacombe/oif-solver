# solver-settlement - Agent Guide

Settlement lifecycle, oracle mechanics, post-fill/pre-claim actions, proof readiness, and claim preparation.

## What lives here

- `SettlementInterface` implementations: direct, broadcaster, Hyperlane.
- Fill-proof validation, post-fill fee quoting, settlement readiness, pusher actions, and oracle route checks.
- Settlement-specific transaction generation before handing payloads to `solver-delivery`.

Order-standard validation belongs in `solver-order`; low-level submission and receipt polling belong in `solver-delivery`; lifecycle scheduling belongs in `solver-core`.

## Readiness and retry semantics

Keep retryable infrastructure states separate from permanent validation failures:

- `BackendUnavailable`, `StorageUnavailable`, `FinalityNotReached`, and proof delay states are generally retry/readiness outcomes.
- `ValidationFailed`, `InvalidProof`, and `FillMismatch` are business or data failures unless a caller documents otherwise.

`ensure_scalar_settlement_supported()` protects the current scalar proof/claim pipeline. Do not remove it when adding multi-fill order generation; add multi-fill settlement support first.

## Verification

```
cargo test -p solver-settlement
cargo check -p solver-settlement
```
