# Nuthatch Gateway

`nuthatch-gateway` is the provider-side HTTP gateway for Nuthatch Data Service.
It is deliberately not a transparent proxy, and it takes some care not to
compose one by accident. It exposes one configured Nuthatch nest through a small,
NID-addressed API, validates TAP v2 receipts for billable requests, checks
on-chain that those receipts can actually be paid, stores them in Postgres, and
leaves Nuthatch's admin, metrics, arbitrary SQL, and every other internal route
private.

```text
consumer
  |  TAP-Receipt
  v
nuthatch-gateway  -- Postgres receipt store
  |
  +-- Horizon core: receipt validation, replay prevention, RAV aggregation, collect()
  +-- payment gate: isAuthorized(payer, signer) + escrow getBalance, via eth_call
  v
private Nuthatch runtime
```

## Why the router is hand-assembled

`horizon_core::run_with` merges your routes *alongside* a catch-all
`/{*path}` transparent proxy. Anything the narrow routes do not claim then goes
straight to the upstream with its path and query string intact — which defeats
the NID check, the query-mode check, and, for any path the pricing policy does
not recognise, the price.

So this crate does not call `run_with`. It builds `AppState` with
`build_state_with`, attaches the payment gate, and assembles its own router with
an explicit `404` fallback and its own `tower_governor` layer. The fallback sits
inside the rate limiter, so probing for unlisted paths is throttled the same as
asking for real ones.

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

Prices match the route shape, not a substring of the path: `/q/sql_top_indexers`
is a 1 CU named query. A path that is not one of the priced routes has no price
at all — `u128::MAX`, which no receipt can satisfy — rather than a free one.

Upstream responses are relayed only up to 8 MiB. Beyond that the gateway returns
`502` rather than buffering an unbounded result under its 512 MiB memory cap.

## The payment gate

A receipt signature proves only that somebody signed. The payer is read from the
receipt's metadata, which the signer controls, so nothing in the signature binds
the two together; and nothing in it says the payer has escrow.
`GraphTallyCollector` checks both when the RAV is redeemed, long after the query
was served.

The `[gate]` section closes that with two `eth_call`s per new payer/signer pair,
cached for `cache_ttl_secs`:

```toml
[gate]
rpc_url = "https://arb1.arbitrum.io/rpc"
payments_escrow = "0xf6Fcc27aAf1fcD8B254498c9794451d82afC673E"
min_escrow_balance = "0"   # required on top of each receipt's value, GRT wei
cache_ttl_secs = 30
rpc_timeout_secs = 5
```

It fails closed. An RPC that cannot answer produces `503`, not an admitted
request. `[tap] authorized_senders = []` means "any signer", which is safe only
with this section present — and the gateway refuses to start when both are
missing.

## Configuration

Copy [`../../deployments/gateway.arbitrum-one.toml.example`](../../deployments/gateway.arbitrum-one.toml.example)
to `gateway.toml` for a host deployment, or
[`../../deployments/gateway.arbitrum-one.compose.toml.example`](../../deployments/gateway.arbitrum-one.compose.toml.example)
for Docker Compose — the two differ in `[server] host` and the database host,
and using the host template in a container yields a gateway nothing can reach.
Fill the provider address, operator key and database password, then set:

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

A per-source-IP limiter covers all NID routes, the free discovery routes, and
the `404` fallback. `/health` and `/ready` intentionally remain unthrottled
for load balancers. The production example uses 10 requests/second and a burst
of 20. Start lower and measure before increasing it.

The upstream URL should point to a loopback-only Nuthatch runtime. For the
first offering it is a mounted Horizon nest at `/horizon`, with the author's
five named queries enabled and both `/sql` and `/explain` refused by Nuthatch.

The templates ship `[collector]` enabled and `tap.aggregator_url` commented
out, which means receipts are stored but no RAV is ever produced and therefore
nothing is collected. Aggregation is the deliberate gate. Do not set
`tap.aggregator_url` until all of the following are true:

1. The data-service proxy has the intended provision floor and a registered provider.
2. The configured provider address and operator key are controlled together.
3. A consumer escrow and authorised signer exist.
4. Receipt to RAV aggregation has been tested against this exact endpoint.
5. A small on-chain `collect()` has been observed and reconciled.
6. `NuthatchDataService.maxCollectableFees(provider)` has headroom for a full
   collection interval. Every `collect()` locks `fees * 5` of provision for
   `minThawingPeriod`; once that exceeds the provision, `collect()` reverts.

## Development and tests

```bash
cargo fmt --all --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
```

The suite covers the composed router — unlisted paths refused rather than
proxied, paid routes gated, `/sql` absent in NAMED mode, foreign NIDs refused,
the rate limiter covering routes and fallback alike, `/health` never throttled —
along with route-shape pricing, NID parsing and matching, query-mode parsing,
safe identifier-to-upstream mapping, the payment gate's cache and requirement
arithmetic, the refusal to start with neither an allowlist nor a gate, and that
every shipped `gateway.toml` template parses as both a horizon-core config and a
Nuthatch one.

The router tests build a real `AppState` over a lazy Postgres pool that is never
connected; every assertion is decided before the pool or the upstream is touched.
The repository CI also validates Compose and runs the Solidity contract suite.
