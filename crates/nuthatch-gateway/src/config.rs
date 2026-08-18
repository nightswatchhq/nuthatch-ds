//! Nuthatch-specific gateway configuration.
//!
//! `horizon-core` owns `gateway.toml` and ignores sections it does not know
//! about, so this module re-reads the same file for the `[gate]` section and
//! then enforces the invariant that keeps the gateway from serving data it will
//! never be paid for.

use alloy_primitives::Address;
use anyhow::{Context, Result, bail};
use horizon_core::Config;
use serde::{Deserialize, Deserializer};

/// Parse a GRT wei amount written either as a TOML integer or, for values past
/// 2^63, as a string. Mirrors how `horizon-core` reads `min_collect_value`.
fn de_u128<'de, D: Deserializer<'de>>(deserializer: D) -> Result<u128, D::Error> {
    use serde::de::Error;

    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Raw {
        Int(i64),
        Str(String),
    }

    match Raw::deserialize(deserializer)? {
        Raw::Int(n) => u128::try_from(n).map_err(|_| D::Error::custom("negative GRT wei amount")),
        Raw::Str(s) => s
            .trim()
            .replace('_', "")
            .parse::<u128>()
            .map_err(D::Error::custom),
    }
}

#[derive(Debug, Deserialize, Clone, Default)]
pub struct NuthatchConfig {
    /// On-chain pre-forward checks. Omit the section to disable them.
    pub gate: Option<GateConfig>,
}

/// Configuration for the on-chain payment gate.
///
/// Receipt signatures prove only that *somebody* signed. They do not prove the
/// named payer authorised that signer, and they do not prove the payer has any
/// escrow to pay from. Both are `eth_call`s, and both are cheap next to serving
/// a query that can never be collected.
#[derive(Debug, Deserialize, Clone)]
pub struct GateConfig {
    /// Read-only RPC endpoint for the pre-forward checks.
    pub rpc_url: String,
    /// Horizon `PaymentsEscrow`.
    /// Arbitrum One: `0xf6Fcc27aAf1fcD8B254498c9794451d82afC673E`.
    /// Arbitrum Sepolia: `0x09B985a2042848A08bA59060EaF0f07c6F5D4d54`.
    pub payments_escrow: Address,
    /// Escrow balance a payer must hold on top of the receipt value, in GRT wei.
    /// A non-zero floor stops a payer draining their last wei mid-session.
    #[serde(default, deserialize_with = "de_u128")]
    pub min_escrow_balance: u128,
    /// How long a successful check stays good for. Bounds both RPC load and how
    /// stale a balance may be when it admits a request.
    #[serde(default = "default_cache_ttl_secs")]
    pub cache_ttl_secs: u64,
    /// Per-call RPC timeout. A gate that hangs is a gateway that hangs.
    #[serde(default = "default_rpc_timeout_secs")]
    pub rpc_timeout_secs: u64,
}

fn default_cache_ttl_secs() -> u64 {
    30
}

fn default_rpc_timeout_secs() -> u64 {
    5
}

impl NuthatchConfig {
    /// Load from the path in `$GATEWAY_CONFIG`, defaulting to `gateway.toml` —
    /// the same file and the same default as [`horizon_core::Config::load`].
    pub fn load() -> Result<Self> {
        let path = std::env::var("GATEWAY_CONFIG").unwrap_or_else(|_| "gateway.toml".to_owned());
        Self::load_from(&path)
    }

    pub fn load_from(path: &str) -> Result<Self> {
        let contents = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read config from {path}"))?;
        Self::parse(&contents)
    }

    pub fn parse(contents: &str) -> Result<Self> {
        toml::from_str(contents).context("failed to parse the [gate] configuration")
    }
}

/// Refuse to start a gateway that would serve paid routes to anybody who can
/// sign sixty-five bytes.
///
/// A receipt is payable only if the payer authorised its signer on
/// `GraphTallyCollector` and has escrow behind it. With no `[gate]` section
/// nothing checks either, so the `[tap] authorized_senders` allowlist is the
/// only thing standing between the upstream and the open internet — and an
/// empty allowlist means "accept any signer".
pub fn validate(core: &Config, nuthatch: &NuthatchConfig) -> Result<()> {
    if core.tap.authorized_senders.is_empty() && nuthatch.gate.is_none() {
        bail!(
            "refusing to start: [tap] authorized_senders is empty and no [gate] section is \
             configured, so every paid route would be served to any receipt signer for free. \
             Add a [gate] section for on-chain escrow checks, or list the consumer addresses \
             you intend to serve in [tap] authorized_senders."
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const CORE: &str = r#"
        [server]
        host = "127.0.0.1"
        port = 8090
        [indexer]
        service_provider_address = "0x0000000000000000000000000000000000000001"
        operator_private_key = "0x01"
        [tap]
        data_service_address = "0x0000000000000000000000000000000000000002"
        authorized_senders = []
        [backend]
        upstream_url = "http://127.0.0.1:8110/horizon"
        [database]
        url = "postgres://nuthatch:pw@127.0.0.1:5432/nuthatch_gateway"
    "#;

    fn core_config(extra: &str) -> Config {
        toml::from_str(&format!("{CORE}\n{extra}")).unwrap()
    }

    #[test]
    fn a_gate_section_is_parsed_with_sensible_defaults() {
        let parsed = NuthatchConfig::parse(
            r#"
            [gate]
            rpc_url = "https://arb1.arbitrum.io/rpc"
            payments_escrow = "0xf6Fcc27aAf1fcD8B254498c9794451d82afC673E"
            "#,
        )
        .unwrap();
        let gate = parsed.gate.unwrap();
        assert_eq!(gate.min_escrow_balance, 0);
        assert_eq!(gate.cache_ttl_secs, 30);
        assert_eq!(gate.rpc_timeout_secs, 5);
    }

    #[test]
    fn escrow_floors_beyond_u64_are_accepted_as_strings() {
        let parsed = NuthatchConfig::parse(
            r#"
            [gate]
            rpc_url = "https://arb1.arbitrum.io/rpc"
            payments_escrow = "0xf6Fcc27aAf1fcD8B254498c9794451d82afC673E"
            min_escrow_balance = "100_000_000_000_000_000_000"
            "#,
        )
        .unwrap();
        assert_eq!(
            parsed.gate.unwrap().min_escrow_balance,
            100_000_000_000_000_000_000
        );
    }

    #[test]
    fn an_open_allowlist_with_no_gate_is_refused() {
        let error = validate(&core_config(""), &NuthatchConfig::default()).unwrap_err();
        assert!(error.to_string().contains("authorized_senders is empty"));
    }

    #[test]
    fn an_open_allowlist_is_fine_once_the_gate_enforces_escrow() {
        let nuthatch = NuthatchConfig::parse(
            r#"
            [gate]
            rpc_url = "https://arb1.arbitrum.io/rpc"
            payments_escrow = "0xf6Fcc27aAf1fcD8B254498c9794451d82afC673E"
            "#,
        )
        .unwrap();
        validate(&core_config(""), &nuthatch).unwrap();
    }

    #[test]
    fn an_explicit_allowlist_is_fine_without_a_gate() {
        let core = core_config("");
        let mut core = core;
        core.tap.authorized_senders = vec![Address::repeat_byte(0xAB)];
        validate(&core, &NuthatchConfig::default()).unwrap();
    }

    /// The shipped templates are the file an operator actually copies. One
    /// `gateway.toml` is read twice — once by horizon-core and once by this
    /// module — and a template that only half-parses is a boot failure on a
    /// production host rather than a red test.
    #[test]
    fn every_shipped_template_loads_and_passes_validation() {
        let root = concat!(env!("CARGO_MANIFEST_DIR"), "/../..");
        for template in [
            "gateway.example.toml",
            "deployments/gateway.arbitrum-one.toml.example",
            "deployments/gateway.arbitrum-one.compose.toml.example",
        ] {
            let path = format!("{root}/{template}");
            let contents = std::fs::read_to_string(&path).expect(&path);

            let core: Config = toml::from_str(&contents)
                .unwrap_or_else(|e| panic!("{template} is not a valid horizon-core config: {e}"));
            let nuthatch = NuthatchConfig::parse(&contents)
                .unwrap_or_else(|e| panic!("{template} has an invalid [gate] section: {e}"));

            assert!(
                nuthatch.gate.is_some(),
                "{template} ships without a [gate] section"
            );
            validate(&core, &nuthatch)
                .unwrap_or_else(|e| panic!("{template} would be refused at startup: {e}"));
        }
    }

    /// The two Arbitrum One templates differ in exactly the two ways that make
    /// one of them unreachable in the other's deployment.
    #[test]
    fn the_compose_template_binds_an_interface_docker_can_publish() {
        let root = concat!(env!("CARGO_MANIFEST_DIR"), "/../..");
        let read = |name: &str| -> Config {
            toml::from_str(&std::fs::read_to_string(format!("{root}/{name}")).unwrap()).unwrap()
        };

        let host = read("deployments/gateway.arbitrum-one.toml.example");
        assert_eq!(host.server.host, "127.0.0.1");
        assert!(host.database.url.contains("@127.0.0.1:5432/"));

        let compose = read("deployments/gateway.arbitrum-one.compose.toml.example");
        assert_eq!(
            compose.server.host, "0.0.0.0",
            "a container binding loopback is unreachable through a published port"
        );
        assert!(
            compose.database.url.contains("@postgres:5432/"),
            "the Compose gateway reaches Postgres by service name"
        );
    }
}
