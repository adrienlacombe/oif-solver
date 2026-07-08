# solver-account - Agent Guide

Account abstraction and signer construction for solver identities.

## What lives here

- `AccountInterface`, `AccountService`, signer wrappers, and account implementation factories.
- Account config parsing and key validation for local, AWS KMS, and Starknet local signers.
- Signer return types consumed by `solver-delivery`.

Transaction construction, fee policy, and submission belong in `solver-delivery`; API or seed wiring belongs in `solver-service`.

## Adding an account backend

Implement `AccountInterface`, provide a `ConfigSchema`, add a `Registry`, and register it in `get_all_implementations()`.

EVM signers use Alloy signer types. Starknet signers use the existing `starknet-rust-*` crates. Do not introduce ethers-rs/web3 for EVM signing.

## Features and verification

- `testing` enables mockall.
- `kms` enables the AWS KMS signer path.

Default check:

```
cargo test -p solver-account
cargo check -p solver-account
```
