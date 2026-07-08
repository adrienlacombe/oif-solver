# solver-delivery - Agent Guide

Low-level transaction delivery across supported chains.

## What lives here

- Chain-specific submission, receipt polling, log querying, gas/native balance checks, nonce handling, and delivery error classification.
- `DeliveryInterface`, `DeliveryService`, implementation registries, and transaction-attempt recorder integration.
- EVM implementation under `implementations/evm`; Starknet implementation under `implementations/starknet.rs`.

Order standards build execution payloads in `solver-order`; settlement backends decide post-fill/pre-claim/claim mechanics in `solver-settlement`; orchestration lives in `solver-core`.

## Retry and replacement invariants

Same-nonce replacement and tx bumping depend on `TransactionAttempt`, `PlannedAttemptInit`, `replacement_of`, and precise `DeliveryError` variants. Do not collapse `NonceTooLow`, `ReplacementUnderpriced`, reverted receipts, pending confirmations, and RPC failures into a generic error unless all callers are updated.

## Chain SDKs

EVM delivery uses Alloy. Starknet delivery uses the workspace `starknet-rust-*` crates. Do not introduce ethers-rs/web3 for EVM paths.

## Verification

```
cargo test -p solver-delivery
cargo check -p solver-delivery
```
