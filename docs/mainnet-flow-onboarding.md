# Mainnet Flow Onboarding

This is the operator-facing package for attracting Ethereum <> Starknet order flow to the
mainnet solver.

## Live Endpoint

- Root URL: `http://oif-solver-mainnet-786829271.eu-west-3.elb.amazonaws.com`
- Public API URL: `http://oif-solver-mainnet-786829271.eu-west-3.elb.amazonaws.com/api/v1`
- Solver ID: `oif-solver-ethereum-starknet-mainnet`
- Region: `eu-west-3`

Public endpoints:

- `GET /health`
- `GET /api/v1/assets`
- `POST /api/v1/quotes`
- `POST /api/v1/orders`
- `GET /api/v1/orders/{id}`

Current live asset surface from `GET /api/v1/assets`:

| Chain | Chain ID | Asset | Address | Decimals |
|---|---:|---|---|---:|
| Ethereum | `1` | STRK | `0xca14007eff0db1f8135f4c25b34de49ab0d42766` | 18 |
| Starknet | `358974494` | ETH | `0x049d36570d4e46f48e99674bd3fcc84644ddd6b96f7c741b1562b82f9e004dc7` | 18 |

## Production Proof

Real mainnet Ethereum <> Starknet flow was executed and the live order API returns it as
`finalized`:

- Order ID: `0x82bb66d9a22779cedc1a1c3937b264d8cc02d175f3165395042313b743181bda`
- Settlement standard: `hyperlane7683`
- Fill transaction: `0x89267b8a708932775a185a9312498d63d0aba2e4d326da16cdc31a1d583bf5b5`

Probe:

```bash
scripts/flow/probe-mainnet-api.sh
```

## Integration Packet

Send this to wallets, aggregators, and order-flow sources:

```text
Solver: oif-solver-ethereum-starknet-mainnet
Route: Ethereum <> Starknet
Live API: http://oif-solver-mainnet-786829271.eu-west-3.elb.amazonaws.com/api/v1
Health: http://oif-solver-mainnet-786829271.eu-west-3.elb.amazonaws.com/health
Supported assets: Ethereum STRK, Starknet ETH
Settlement: Hyperlane/OIF Starknet, ERC-7683/OIF-compatible direct order handling
Proof order: 0x82bb66d9a22779cedc1a1c3937b264d8cc02d175f3165395042313b743181bda
Proof fill: 0x89267b8a708932775a185a9312498d63d0aba2e4d326da16cdc31a1d583bf5b5
```

## LI.FI Solver Marketplace

Current LI.FI docs describe the solver path as:

1. Register a solver account in the solver UI.
2. Generate an API key.
3. Register/prove ownership of the solver fill addresses.
4. Push standing quotes to `POST https://order.li.fi/quotes/submit`.
5. Receive orders through their WebSocket/order-flow surface or on-chain monitoring.

Use:

```bash
LIFI_SOLVER_API_KEY=... \
EXCLUSIVE_FOR=0xYOUR_REGISTERED_SOLVER_ADDRESS \
DRY_RUN=0 \
scripts/flow/lifi-submit-quotes.sh
```

The `oif-solver-mainnet/lifi-solver-api-key` secret exists in AWS Secrets Manager. LI.FI reports
`0xd4a1A11fb69c906D82EC7D99e91a28fc62447415` under `GET /solver-api/solver/identities`; use that
address as `EXCLUSIVE_FOR`.

Current Step 3 status: LI.FI accepts the registered identity but does not yet accept Ethereum <->
Starknet quote routes. `GET /chains/supported` does not list Starknet, V1 quote requests reject
`starknet:*` CAIP-2 namespaces, and quote submission rejects 32-byte Starknet asset addresses.
Ask LI.FI to enable Starknet in the LI.FI Intent protocol before publishing standing quotes.

The account currently has no issued ACM certificate or Route53 hosted zone visible through the
`alc` AWS profile. Those are external onboarding items, not code changes.

## Quote Competitiveness

The quote publisher defaults to conservative route ranges. Tune these from live inventory and
realized PnL:

- `MIN_AMOUNT` (raw source-asset base units)
- `MAX_AMOUNT` (raw source-asset base units)
- `QUOTE_RATE`
- `EXPIRY_SECONDS`

Operationally, keep these dashboards/alarms green before scaling flow:

- ALB 5xx
- target 5xx
- unhealthy targets
- p99 target latency
- ECS CPU and memory
- ECS running task shortage

## Route Expansion

Do not advertise assets that are not funded and settlement-tested. The next commercially useful
assets for Ethereum <> Starknet are:

- ETH/WETH
- USDC
- STRK

Add them only after confirming token addresses, decimals, solver balances, fill support, claim
support, and at least one real mainnet fill per direction.
