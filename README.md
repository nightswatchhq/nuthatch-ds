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

## Run on Arbitrum Sepolia

1. Copy `.env.example` to `.env`, fill `PRIVATE_KEY`, `OWNER`, `PAUSE_GUARDIAN`,
   `NUTHATCH_NID`, and `NUTHATCH_QUERY_MODE`.
2. Deploy the UUPS proxy:

   ```bash
   forge script contracts/script/Deploy.s.sol --rpc-url arbitrum_sepolia \
     --private-key $PRIVATE_KEY --broadcast --verify -vvvv
   ```

3. Copy `gateway.example.toml` to `gateway.toml`, set the deployed proxy as
   `tap.data_service_address`, configure the provider address and operator key, and point
   `backend.upstream_url` at the private Nuthatch runtime. For `docker compose`, use
   `postgres` rather than `localhost` as the database host.
4. Provision GRT, then call `register` followed by `startService` with:

   ```solidity
   abi.encode(nid, INuthatchDataService.QueryMode.NAMED, "https://your-endpoint")
   ```

5. Start Postgres and the gateway with `docker compose up`.

The first intended offering is the NID of
[`nightswatchhq/horizon-nest`](https://github.com/nightswatchhq/horizon-nest), served in
`NAMED` mode.
