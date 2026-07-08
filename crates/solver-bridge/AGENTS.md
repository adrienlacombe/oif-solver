# solver-bridge - Agent Guide

Cross-chain rebalance bridge orchestration. This crate manages rebalance transfers, bridge state, cooldowns, pair locks, and the automated threshold monitor.

## What lives here

- `BridgeInterface` and protocol-specific bridge implementations.
- `BridgeService`, transfer state transitions, cooldown logic, and pending-transfer reconciliation.
- `RebalanceMonitor`, threshold evaluation, and `BridgeStorage` helpers.

General order settlement logic belongs in `solver-settlement`; transaction submission belongs in `solver-delivery`; operator config shapes belong in `solver-types`.

## Failure semantics

Preserve the distinction between retryable and terminal bridge outcomes:

- `ApprovePending` and `NonceTooLow` are retryable/reconcilable.
- `TransactionFailed` and `ApproveReverted` are terminal unless a caller explicitly records intervention state.
- Pair locks and cooldowns prevent repeated transfers from compounding risk; do not bypass them in monitor paths.

## Verification

```
cargo test -p solver-bridge
cargo check -p solver-bridge
```
