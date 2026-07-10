# Cairo dependencies

This is the OIF Starknet (Hyperlane7683) Cairo project, imported from
`adrien-oif-starknet/cairo` for maintenance alongside the solver.

## hyperlane-starknet

The `contracts`, `mocks`, and `token` crates are git dependencies (see
`Scarb.toml`) pointing at the **OIF fork**:

- Repo: <https://github.com/adrienlacombe/hyperlane-starknet>
- Branch: `main`
- Pinned rev: `88cd98fcf25f9a9c1e97fa0dc99416d0c55242cb`

That fork is upstream [`hyperlane-xyz/hyperlane-starknet`](https://github.com/hyperlane-xyz/hyperlane-starknet)
at `eb57b8d3017d9cbfe47ffbdb3b329b06b4356d1d` plus **one** patch:

- `cairo/crates/contracts/src/interfaces.cairo`: `ETH_ADDRESS()` returns Starknet
  **STRK** (`0x04718f5a0fc34cc1af16a1cdee98ffb20c31f5cd61d6ab07201858f4287c938d`)
  instead of Starknet ETH. The production Starknet mailbox charges STRK for
  dispatch fees, so the router must approve/transfer STRK for dispatch.

## Updating the fork

To move to a newer upstream base or change the patch:

1. In the fork's `main`, re-apply the `ETH_ADDRESS` patch on top of the new
   upstream revision and push.
2. Update the three `rev = "…"` entries in `Scarb.toml` to the new commit.
3. Rebuild with the pinned toolchain: `scarb build` (scarb 2.10.1, see
   `.tool-versions`). A newer scarb fails on dependency trait/snapshot changes.
