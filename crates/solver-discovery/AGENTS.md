# solver-discovery - Agent Guide

Intent discovery from on-chain sources, off-chain sources, and trusted in-process submissions.

## What lives here

- `DiscoveryInterface`, discovery implementation factories, and `DiscoveryService`.
- On-chain event monitoring under `implementations/onchain`.
- Off-chain and in-process submission adapters under `implementations/offchain`.

Validation of public HTTP requests belongs in `solver-service`; order-standard validation belongs in `solver-order`; lifecycle orchestration belongs in `solver-core`.

## Trust boundary

`DiscoveryInterface::submit_order` is not a public validation boundary. The sanctioned public caller is `solver-service` after `validate_intent_request` has checked sponsor signatures, allocator authorization, and capacity. Do not call this path with unvalidated user input.

## Adding a discovery source

Implement `DiscoveryInterface`, provide a `ConfigSchema`, add a `Registry`, register it in `get_all_implementations()`, and make `stop_monitoring()` release spawned tasks/resources cleanly.

## Verification

```
cargo test -p solver-discovery
cargo check -p solver-discovery
```
