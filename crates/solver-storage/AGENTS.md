# solver-storage - Agent Guide

Persistence abstractions and storage backends.

## What lives here

- Low-level byte storage via `StorageInterface`.
- `StorageService`, `ConfigStore`, nonce storage, compact reservations, Redis health/readiness checks.
- Memory, file, and Redis backend implementations.

Domain state shapes belong in `solver-types`; service-level storage selection belongs in `solver-service`; bridge-specific persistence helpers live in `solver-bridge`.

## Backend semantics

Do not assume all backends support the same query filters. Redis intentionally rejects `NotEquals` and `NotIn` because they require unbounded namespace scans. If shared code needs a query, design it around positive indexed filters or add an explicit backend-specific path.

Atomic operations (`set_nx`, `compare_and_swap`, `compare_and_swap_with_indexes`, `delete_if_exists`) are correctness boundaries for config seeding, optimistic locking, and recovery. Preserve index refresh behavior when changing CAS paths.

## Tests

Default tests should not require Redis. Redis-dependent tests are ignored unless explicitly selected; `cluster-tests` require Docker and local cluster setup.

```
cargo test -p solver-storage
cargo check -p solver-storage
```
