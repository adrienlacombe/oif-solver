# solver-order - Agent Guide

Order-standard validation, execution decisions, and fill/claim transaction generation.

## What lives here

- `OrderInterface` implementations for standards such as EIP-7683 and Hyperlane 7683.
- Execution strategies implementing `ExecutionStrategy`.
- Order validation, order-id construction callbacks, and standard-specific execution payload generation.

Discovery sources belong in `solver-discovery`; transaction submission belongs in `solver-delivery`; settlement proof/readiness handling belongs in `solver-settlement`.

## Execution payloads

EVM standards may use the scalar `Transaction` helpers. Non-EVM or multi-leg standards should use the typed `ExecutionTransaction` methods instead of forcing data through EVM-only shapes.

Scalar settlement is still guarded in `solver-settlement`; if a standard can produce multiple fill transactions, update settlement support deliberately rather than only changing order generation.

## Adding a standard or strategy

Add the implementation under `implementations/standards` or `implementations/strategies`, provide a `Registry`, and register it in the relevant `get_all_*_implementations()` function.

## Verification

```
cargo test -p solver-order
cargo check -p solver-order
```
