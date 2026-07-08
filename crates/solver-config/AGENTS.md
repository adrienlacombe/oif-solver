# solver-config - Agent Guide

Bootstrap configuration model, parsing, validation, and builders.

## Boundaries

- Static config structs and validation live here.
- Runtime seed/env merging lives in `solver-service/src/config_merge.rs`.
- Persisted operator config shapes live in `solver-types/src/operator_config.rs`.
- Config storage and optimistic locking live in `solver-storage`.

When adding config fields, check whether the same field also needs a seed override, an operator-config field, service merge logic, and example config updates.

## Compatibility

Use `#[serde(default)]` for newly optional fields. Do not silently change defaults for production-sensitive settings such as finality, tx bumping, storage, signing, settlement, or rebalancing.

## Verification

```
cargo test -p solver-config
cargo check -p solver-config
```
