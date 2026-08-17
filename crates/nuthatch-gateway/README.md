# Nuthatch Gateway

`nuthatch-gateway` is the provider-side HTTP gateway for Nuthatch Data Service.
It is deliberately not a transparent proxy. It exposes one configured Nuthatch
nest through a small, NID-addressed API, validates TAP v2 receipts for billable
requests, stores accepted receipts in Postgres, and leaves Nuthatch's admin,
metrics, arbitrary SQL, and every other internal route private.

```text
consumer
  |  TAP-Receipt
  v
nuthatch-gateway  -- Postgres receipt store
  |
  +-- Horizon core: receipt validation, replay prevention, RAV aggregation, collect()
  v
private Nuthatch runtime
```

## Public surface

The gateway serves exactly one NID, configured through `NUTHATCH_NID`.

| Route | Receipt | Price | Behaviour |
| --- | --- | --- | --- |
| `GET /health` | no | free | process liveness |
| `GET /ready` | no | free | Postgres readiness |
| `GET /v1/nests/:nid/schema` | no | free | schema discovery |
| `GET /v1/nests/:nid/queries` | no | free | named-query discovery |
| `GET /v1/nests/:nid/q/:query` | yes | 1 CU | named query |
| `GET /v1/nests/:nid/table/:table` | yes | 2 CU | table read |
| `GET /v1/nests/:nid/sql?q=...` | yes | 20 CU | SQL offering only |

`NUTHATCH_QUERY_MODE=NAMED` is the default. In that mode `/sql` answers `404`.
The upstream must independently run Nuthatch with `sql = "allowlist"`. The
gateway is a payment and routing boundary, not a substitute for a bounded query
surface.

Names used in `/q/:query` and `/table/:table` accept only letters, digits,
underscores and hyphens. This prevents an external path segment becoming an
unreviewed upstream path.

## Configuration

Copy [`../../deployments/gateway.arbitrum-one.toml.example`](../../deployments/gateway.arbitrum-one.toml.example)
to `gateway.toml`, fill the provider address, operator key and database
password, then set:

```bash
export GATEWAY_CONFIG=$PWD/gateway.toml
export NUTHATCH_NID=36d3c71446a56cdb5b90536d3f5f77351b1d92efcca94bc2fd41b1c368e69410
export NUTHATCH_QUERY_MODE=NAMED
cargo run --release --bin nuthatch-gateway
```

The data-service proxy is
`0x647D1Fd14AF2DE3947522B74F1de5B99d317c10A` on Arbitrum One. The gateway
configuration does not register a provider or move GRT. It only describes the
provider which will eventually do so.

For the production topology, run Postgres with Compose and the gateway as the
host systemd unit at
[`../../deployments/nuthatch-ds-gateway.service`](../../deployments/nuthatch-ds-gateway.service).
This gives both the gateway and Nuthatch a loopback-only connection without
Docker bridge forwarding. Keep `/etc/nuthatch-ds/gateway.toml` mode 0600.
Compose binds Postgres only to `127.0.0.1`; Caddy is the only public entry
point in front of the gateway.

## Rate limiting and operations

`horizon-core` applies a per-source-IP limiter to all NID routes, including
free discovery routes. `/health` and `/ready` intentionally remain unthrottled
for load balancers. The production example uses 10 requests/second and a burst
of 20. Start lower and measure before increasing it.

The upstream URL should point to a loopback-only Nuthatch runtime. For the
first offering it is a mounted Horizon nest at `/horizon`, with the author's
five named queries enabled and both `/sql` and `/explain` refused by Nuthatch.

Do not enable `[collector]` or `tap.aggregator_url` until all of the following
are true:

1. The data-service proxy has the intended provision floor and a registered provider.
2. The configured provider address and operator key are controlled together.
3. A consumer escrow and authorised signer exist.
4. Receipt to RAV aggregation has been tested against this exact endpoint.
5. A small on-chain `collect()` has been observed and reconciled.

## Development and tests

```bash
cargo fmt --check
cargo test --locked
```

The unit suite covers NID parsing and matching, query-mode parsing, safe
identifier-to-upstream mapping, pricing, and rejection of malformed NIDs and
route segments. The repository CI also validates Compose and runs the Solidity
contract suite.
