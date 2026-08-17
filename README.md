# Nuthatch Data Service

Paid access to reproducible, self-hosted [Nuthatch](https://github.com/nightswatchhq/nuthatch)
indexed datasets on Graph Horizon.

The Arbitrum One proxy deployment is
[`0x647D…c10A`](https://arbiscan.io/address/0x647D1Fd14AF2DE3947522B74F1de5B99d317c10A).
See [`deployments/arbitrum-one.json`](deployments/arbitrum-one.json) for the immutable deployment
record.

The deployed proxy currently has a 555 GRT provision floor. The repository prepares a zero-floor
soft-launch upgrade, but that is not effective on-chain until the proxy owner executes it. A
deployment record is history, not an oracle, which is why it says 555 GRT until that transaction
exists.

The unit of service is a Nuthatch nest identity, or NID. A provider advertises a
specific `(NID, QueryMode, endpoint)` offering on-chain. Consumers can therefore choose
an independently reproducible dataset, not merely a server which says that it does SQL.

```
consumer -- TAP-Receipt --> nuthatch-gateway --> private nuthatch :8288
                                  |                  /schema /q /table /sql
                                  +-- receipts -> RAV -> NuthatchDataService.collect()
```

The Solidity service is a UUPS Horizon data service. The Rust gateway is built on
[`horizon-core`](https://github.com/nightswatchhq/horizon-core), which validates TAP
receipts, rejects replayed nonces, persists receipts, aggregates RAVs, and collects them.

## Public API

The public gateway is deliberately narrow. It does not expose Nuthatch administration,
metrics, internal metadata, or its general routing surface.

```
GET /v1/nests/:nid/schema                 free discovery
GET /v1/nests/:nid/queries                free discovery
GET /v1/nests/:nid/q/:query               1 CU, TAP required
GET /v1/nests/:nid/table/:table           2 CU, TAP required
GET /v1/nests/:nid/sql?q=...              20 CU, TAP required, SQL offerings only
```

The gateway is configured with exactly one `NUTHATCH_NID`. It rejects another NID rather
than treating the path as a decorative suggestion. Run another gateway for another
offering. `NUTHATCH_QUERY_MODE=NAMED` is the default. Set it to `SQL` only for an offering
explicitly registered as `QueryMode.SQL`.

`/schema` and `/queries` are intentionally free. They are discovery metadata. The named
query surface is the normal product. Arbitrary SQL is not a public-security boundary in
Nuthatch and is deliberately opt-in.

## Build

```bash
./setup-contracts.sh
forge build && forge test
cargo test
```

`setup-contracts.sh` pins `graphprotocol/contracts` to the Horizon 1.1.0 commit. Do not
casually float it to a later Graph contracts release. The layout changed, naturally.

## Deployment and provider lifecycle

The contract is deployed on Arbitrum One, but a deployment is not a live
service. There is currently no registered provider and no settled paid query.
The first intended offering is the computed NID
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

The proxy currently has its historical 555 GRT provision floor. The source has
a tested zero-floor soft-launch implementation, but UUPS means that change only
exists on-chain after the owner deploys and executes an upgrade. Do not claim a
zero provision floor before that transaction is confirmed.

### Upstream hardening

The Nuthatch runtime must be private and mounted with `sql = "allowlist"`. The
checked-in [`deployments/horizon-runtime.mounts.toml`](deployments/horizon-runtime.mounts.toml)
exposes only the five queries sanctioned by `horizon-nest` and refuses arbitrary
`/sql` and `/explain`. It is designed for the Nuthatch 2.x `[[chains]]` runtime
configuration. The systemd unit at
[`deployments/nuthatch-ds-upstream.service`](deployments/nuthatch-ds-upstream.service)
binds it to loopback and disables the Nuthatch admin UI.

### Gateway deployment

The provider gateway is documented in
[`crates/nuthatch-gateway/README.md`](crates/nuthatch-gateway/README.md). Its
Arbitrum One template is
[`deployments/gateway.arbitrum-one.toml.example`](deployments/gateway.arbitrum-one.toml.example).

For Docker Compose:

```bash
cp deployments/gateway.arbitrum-one.toml.example gateway.toml
chmod 600 gateway.toml
printf 'NUTHATCH_NID=36d3c71446a56cdb5b90536d3f5f77351b1d92efcca94bc2fd41b1c368e69410\n' > .env
printf 'NUTHATCH_QUERY_MODE=NAMED\n' >> .env
printf 'POSTGRES_PASSWORD=<high-entropy-secret>\n' >> .env
chmod 600 .env
docker compose --env-file .env up -d --build
```

Compose exposes only `127.0.0.1:8090` and keeps Postgres on its internal Docker
network. A reverse proxy provides TLS and public DNS. The gateway enforces a
per-IP rate limit for all NID routes. The production default is 10 requests per
second with a burst of 20. It must remain `NAMED` until a separate SQL offering
is deliberately registered and the upstream is reviewed for that exposure.

## Verification

```bash
cargo fmt --check
cargo test --locked
cd contracts && forge fmt --check && forge test -vvv
NUTHATCH_NID=<nid> NUTHATCH_QUERY_MODE=NAMED POSTGRES_PASSWORD=test docker compose config -q
```

The gateway tests cover malformed NIDs, case-normalised NID matching, named and
SQL mode parsing, safe upstream identifiers, pricing, and NID mismatch. The
contract suite covers registration with zero provision, offering activation,
restart/update behaviour, both query modes for one NID, invalid NIDs,
unregistered providers, stop/deregister lifecycle, payment destination defaults,
and the deliberate no-slashing policy.
