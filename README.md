# Nuthatch Data Service

Paid access to reproducible, self-hosted [Nuthatch](https://github.com/nightswatchhq/nuthatch)
indexed datasets on Graph Horizon.

The Arbitrum One proxy deployment is
[`0x647D…c10A`](https://arbiscan.io/address/0x647D1Fd14AF2DE3947522B74F1de5B99d317c10A).
See [`deployments/arbitrum-one.json`](deployments/arbitrum-one.json) for the immutable deployment
record.

The unit of service is a Nuthatch nest identity, or NID. A provider advertises a
specific `(NID, QueryMode, endpoint)` offering on-chain. Consumers can therefore choose
an independently reproducible dataset, not merely a server which says that it does SQL.

```
consumer -- TAP-Receipt --> nuthatch-gateway --> private nuthatch :8110/horizon
                              |    |                 /schema /q /table /sql
                              |    +-- escrow + signer pre-check (eth_call)
                              +-- receipts -> RAV -> NuthatchDataService.collect()
```

The Solidity service is a UUPS Horizon data service. The Rust gateway is built on
[`horizon-core`](https://github.com/nightswatchhq/horizon-core), which validates TAP
receipts, rejects replayed nonces, persists receipts, aggregates RAVs, and collects them.
The `horizon-core` dependency is pinned to a commit rather than tracking `main`.

## Public API

The public gateway is narrow, and the router is what makes it narrow. It names five
routes; everything else is `404`.

```
GET /v1/nests/:nid/schema                 free discovery
GET /v1/nests/:nid/queries                free discovery
GET /v1/nests/:nid/q/:query               1 CU, TAP required
GET /v1/nests/:nid/table/:table           2 CU, TAP required
GET /v1/nests/:nid/sql?q=...              20 CU, TAP required, SQL offerings only
```

Plus `/health` and `/ready`, which are unauthenticated, unpriced, and unthrottled.

This is worth being explicit about, because it is easy to get wrong.
`horizon_core::run_with` composes your routes *alongside* a catch-all transparent
proxy, so any path you do not name is forwarded verbatim to the upstream. This
gateway therefore builds its own router with an explicit `404` fallback and its own
rate limiter, and does not call `run_with`. `unlisted_paths_are_refused_rather_than_proxied`
in [`main.rs`](crates/nuthatch-gateway/src/main.rs) is the test that holds the line.

Pricing matches the route shape, not a substring of the path: a named query called
`sql_top_indexers` costs 1 CU, and a path that is not a priced route has no price at
all rather than a free one.

The gateway is configured with exactly one `NUTHATCH_NID`. It rejects another NID rather
than treating the path as a decorative suggestion. Run another gateway for another
offering. `NUTHATCH_QUERY_MODE=NAMED` is the default. Set it to `SQL` only for an offering
explicitly registered as `QueryMode.SQL`.

`/schema` and `/queries` are intentionally free. They are discovery metadata. The named
query surface is the normal product. Arbitrary SQL is not a public-security boundary in
Nuthatch and is deliberately opt-in.

## Payment, and what a receipt actually proves

A valid TAP receipt proves that *somebody* signed sixty-five bytes over the
GraphTallyCollector domain. It does not prove that the payer named in the receipt's
metadata authorised that signer — the payer field is read out of metadata the signer
controls — and it does not prove that payer holds any escrow. `GraphTallyCollector`
checks both, on-chain, when the RAV is eventually redeemed. That is far too late to
decline the query.

So the gateway checks both before forwarding, in the `[gate]` section of
`gateway.toml`:

```toml
[gate]
rpc_url = "https://arb1.arbitrum.io/rpc"
payments_escrow = "0xf6Fcc27aAf1fcD8B254498c9794451d82afC673E"
min_escrow_balance = "0"
cache_ttl_secs = 30
rpc_timeout_secs = 5
```

Two `eth_call`s — `GraphTallyCollector.isAuthorized(payer, signer)` and
`PaymentsEscrow.getBalance(payer, collector, receiver)` — behind a short TTL cache.
The gate fails closed: if the RPC cannot answer, the request is refused rather than
admitted on trust.

`[tap] authorized_senders` is the other half. An empty list means "accept any signer",
which is a reasonable open-market posture *with* `[gate]` configured and a dangerous
one without it. The gateway refuses to start when both are absent rather than quietly
serving every paid route for nothing.

## Build

```bash
./setup-contracts.sh
forge build && forge test
cargo test
```

`setup-contracts.sh` pins `graphprotocol/contracts` to the Horizon 1.1.0 commit. Do not
casually float it to a later Graph contracts release. The layout changed, naturally.

## Deployment and provider lifecycle

The contract is deployed on Arbitrum One with a zero service-level provision floor: the
555 GRT floor it launched with was migrated to zero in
[this upgrade](https://arbiscan.io/tx/0xdb512643b0eb2f73cbfa4d86d1307abc125739d8e6c007a512c0b89f5291d3b5).
Horizon Staking still requires every provision itself to be non-zero.

The first offering is the computed NID
`36d3c71446a56cdb5b90536d3f5f77351b1d92efcca94bc2fd41b1c368e69410` of
[`nightswatchhq/horizon-nest`](https://github.com/nightswatchhq/horizon-nest),
served in `NAMED` mode.

The on-chain lifecycle is:

```text
provision → register → startService(NID, NAMED, endpoint)
          → validate receipts → aggregate RAVs → collect()
          → stopService → deregister
```

`startService` expects:

```solidity
abi.encode(nid, INuthatchDataService.QueryMode.NAMED, "https://your-endpoint")
```

A provider may hold up to `MAX_OFFERINGS_PER_PROVIDER` (32) distinct `(NID, mode)`
offerings. The array is scanned linearly, and an unbounded one would eventually make
`deregister()` unexecutable.

### Provision headroom

Every `collect()` locks `fees * STAKE_TO_FEES_RATIO` (5×) of the provider's provision
for `minThawingPeriod`, and releases expired claims first. Once the locked total would
exceed the available provision, `collect()` reverts with
`ProvisionTrackerInsufficientTokens`. A thin provision runs out of headroom long before
anything else complains: 0.0001 GRT of provision backs 0.00002 GRT of fees, which at
4×10¹² wei per CU is about five compute units.

`NuthatchDataService.maxCollectableFees(provider)` reports the current headroom in GRT
wei. Monitor it, or size the provision for the collection interval.

### Upstream hardening

The Nuthatch runtime must be private and mounted with `sql = "allowlist"`. The
checked-in [`deployments/horizon-runtime.mounts.toml`](deployments/horizon-runtime.mounts.toml)
exposes only the five queries sanctioned by `horizon-nest` and refuses arbitrary
`/sql` and `/explain`. Every caller-supplied `LIMIT` is clamped with `LEAST({n}, 1000)`:
without it a 1 CU named query accepts an arbitrary row count, and the gateway buffers
the response under a 512 MiB cap. The gateway also refuses to relay any upstream
response over 8 MiB.

It is designed for the Nuthatch 2.x `[[chains]]` runtime configuration. The systemd unit
at [`deployments/nuthatch-ds-upstream.service`](deployments/nuthatch-ds-upstream.service)
binds it to `127.0.0.1:8110` and disables the Nuthatch admin UI.

### Gateway deployment

Two templates, and the difference between them matters:

| Deployment | Template | `[server] host` | Database host |
| --- | --- | --- | --- |
| Host / systemd | [`gateway.arbitrum-one.toml.example`](deployments/gateway.arbitrum-one.toml.example) | `127.0.0.1` | `127.0.0.1:5432` |
| Docker Compose | [`gateway.arbitrum-one.compose.toml.example`](deployments/gateway.arbitrum-one.compose.toml.example) | `0.0.0.0` | `postgres:5432` |

Docker publishes a port to the container's bridge address, not its loopback, so a
container that binds `127.0.0.1` is reachable by nothing at all. Use the right one.

For Docker Compose:

```bash
cp deployments/gateway.arbitrum-one.compose.toml.example gateway.toml
chmod 600 gateway.toml
printf 'NUTHATCH_NID=36d3c71446a56cdb5b90536d3f5f77351b1d92efcca94bc2fd41b1c368e69410\n' > .env
printf 'NUTHATCH_QUERY_MODE=NAMED\n' >> .env
printf 'POSTGRES_PASSWORD=<high-entropy-secret>\n' >> .env
chmod 600 .env
docker compose --env-file .env up -d --build
```

`.env` and `gateway.toml` are excluded from the Docker build context by `.dockerignore`.
The builder stage keeps whatever it is given, and the operator key lives in one of them.

For production, run Postgres with Compose and the gateway using
[`deployments/nuthatch-ds-gateway.service`](deployments/nuthatch-ds-gateway.service).
Both the gateway and the upstream Nuthatch runtime bind only to loopback, and Postgres
binds only to `127.0.0.1`. A reverse proxy provides TLS and public DNS. The gateway
enforces a per-IP rate limit across the public routes and the 404 fallback alike, so
probing for unlisted paths is throttled the same as asking for real ones. The production
default is 10 requests per second with a burst of 20. It must remain `NAMED` until a
separate SQL offering is deliberately registered and the upstream is reviewed for that
exposure.

Neither template ships with `aggregator_url` set. Without it receipts accumulate in
Postgres and no RAV is ever produced to collect. See
[`crates/nuthatch-gateway/README.md`](crates/nuthatch-gateway/README.md).

## Verification

The mainnet beta and its exact checks are described in
[`docs/announcing-nuthatch-data-service.md`](docs/announcing-nuthatch-data-service.md).

```bash
cargo fmt --all --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
cd contracts && forge fmt --check && forge test -vvv
NUTHATCH_NID=<nid> NUTHATCH_QUERY_MODE=NAMED POSTGRES_PASSWORD=test docker compose config -q

# Payment gate against live Arbitrum One (needs network, ignored by default).
NUTHATCH_TEST_RPC_URL=<arbitrum-one-rpc> cargo test --locked -- --ignored
```

The 31 gateway tests cover the composed router (unlisted paths refused rather than
proxied, paid routes gated, SQL absent in NAMED mode, foreign NIDs refused, rate
limiting across routes and fallback, `/health` never throttled), route-shape pricing,
malformed NIDs, safe upstream identifiers, the payment-gate cache and requirement
arithmetic, the refusal to start with neither an allowlist nor a gate, and that every shipped
gateway template parses as both a horizon-core config and a Nuthatch one. Two
further tests, ignored unless `NUTHATCH_TEST_RPC_URL` is set, run the gate's two
`eth_call`s against live Arbitrum One and check them against what the beta left
on-chain.

The 33 contract tests cover registration with zero provision, offering activation, the
offering cap, restart/update behaviour, both query modes for one NID, invalid NIDs,
unregistered providers, stop/deregister lifecycle, exit while paused, payment
destination defaults, fee withdrawal and its access control, the thawing-period setter
moving both stores, the deliberate no-slashing policy, and `collect()` — burn and
retention split, stake locking and release, provision headroom exhaustion, payment-type
and service-provider validation, and refusal while paused.
