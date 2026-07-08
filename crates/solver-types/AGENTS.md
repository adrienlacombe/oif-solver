# solver-types - Agent Guide

Shared data contracts for the workspace.

## What lives here

- Public structs/enums for orders, events, auth, storage, pricing, networks, operators, standards, and transaction attempts.
- Chain-agnostic execution data such as `ExecutionTransaction`, `NetworkKind`, and address/hash wrappers.
- Serde shapes persisted in storage or exposed over the HTTP API.

Implementations with side effects belong in the specialized crates. Keep this crate focused on data contracts, validation helpers, conversions, and feature-gated interface bindings.

## Cross-crate impact

Changing a public type here usually affects most of the workspace. Update re-exports in `src/lib.rs`, builders under `src/utils/tests/builders`, config seed overrides, OpenAPI/API structs, and any storage serialization expectations as needed.

For optional persisted/config fields, use serde defaults deliberately. Avoid renaming serialized fields without a clean drain/migration plan.

## Features

The default feature `oif-interfaces` enables Alloy Solidity interface bindings. Keep feature-gated interface code optional so pure type consumers can still build without unnecessary contract bindings.

## Verification

After changes here, prefer the broader check:

```
cargo check --all-targets --all-features
```

For narrow changes, also run:

```
cargo test -p solver-types
```
