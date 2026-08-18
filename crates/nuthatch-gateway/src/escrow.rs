//! On-chain payment gate.
//!
//! A valid TAP receipt proves that somebody signed sixty-five bytes over the
//! GraphTallyCollector domain. It does not prove that the payer named in the
//! receipt's metadata authorised that signer, and it does not prove the payer
//! has any escrow to pay from. Both are what `GraphTallyCollector.collect()`
//! will check on-chain, weeks later, when the RAV is finally redeemed — by
//! which point the query has long since been served for nothing.
//!
//! So we check both here, before forwarding, with two `eth_call`s and a short
//! TTL cache. The gate fails closed: if the RPC cannot answer, the request is
//! refused rather than admitted on trust.

use std::{
    collections::HashMap,
    sync::Mutex,
    time::{Duration, Instant},
};

use alloy::{
    providers::{ProviderBuilder, RootProvider},
    sol,
    transports::http::{Client, Http},
};
use alloy_primitives::{Address, U256};
use anyhow::{Context, Result};
use axum::http::StatusCode;
use horizon_core::{
    Config,
    gate::{GateRejection, GateResult, RequestGate},
    tap::ValidatedReceipt,
};

use crate::config::GateConfig;

type HttpProvider = RootProvider<Http<Client>>;

sol! {
    #[sol(rpc)]
    interface IPaymentsEscrow {
        function getBalance(address payer, address collector, address receiver)
            external view returns (uint256);
    }

    #[sol(rpc)]
    interface IAuthorizable {
        function isAuthorized(address authorizer, address signer) external view returns (bool);
    }
}

/// A cached verdict: this payer/signer pair was authorised at `checked_at`, and
/// the payer held `balance` in escrow at that moment.
#[derive(Clone, Copy)]
struct Verdict {
    checked_at: Instant,
    balance: u128,
}

pub struct EscrowGate {
    provider: HttpProvider,
    escrow: Address,
    /// GraphTallyCollector — both the escrow key and the authorisation registry.
    collector: Address,
    /// This provider's on-chain address; the escrow receiver.
    receiver: Address,
    min_balance: u128,
    ttl: Duration,
    timeout: Duration,
    cache: Mutex<HashMap<(Address, Address), Verdict>>,
}

impl EscrowGate {
    /// Build the gate. `core` supplies the GraphTallyCollector address (the
    /// EIP-712 verifying contract) and this provider's own address, so the gate
    /// cannot drift out of step with the receipts it is checking.
    pub fn new(gate: &GateConfig, core: &Config) -> Result<Self> {
        let url: reqwest::Url = gate
            .rpc_url
            .parse()
            .with_context(|| format!("invalid [gate] rpc_url: {}", gate.rpc_url))?;

        Ok(Self {
            provider: ProviderBuilder::new().on_http(url),
            escrow: gate.payments_escrow,
            collector: core.tap.eip712_verifying_contract,
            receiver: core.indexer.service_provider_address,
            min_balance: gate.min_escrow_balance,
            ttl: Duration::from_secs(gate.cache_ttl_secs),
            timeout: Duration::from_secs(gate.rpc_timeout_secs),
            cache: Mutex::new(HashMap::new()),
        })
    }

    /// Escrow a payer must hold for this receipt: the receipt's own value, plus
    /// the configured floor.
    fn required_balance(&self, receipt_value: u128) -> u128 {
        receipt_value.saturating_add(self.min_balance)
    }

    fn cached(&self, payer: Address, signer: Address) -> Option<Verdict> {
        let cache = self.cache.lock().ok()?;
        let verdict = *cache.get(&(payer, signer))?;
        (verdict.checked_at.elapsed() < self.ttl).then_some(verdict)
    }

    fn remember(&self, payer: Address, signer: Address, balance: u128) {
        if let Ok(mut cache) = self.cache.lock() {
            // The cache is keyed by a pair the payer controls, so bound it. A
            // provider serving more than this many distinct pairs inside one TTL
            // window is being probed, not used.
            if cache.len() >= 4096 {
                cache.clear();
            }
            cache.insert(
                (payer, signer),
                Verdict {
                    checked_at: Instant::now(),
                    balance,
                },
            );
        }
    }

    async fn is_authorized(&self, payer: Address, signer: Address) -> Result<bool, GateRejection> {
        let collector = IAuthorizable::new(self.collector, &self.provider);
        let call = collector.isAuthorized(payer, signer);
        match tokio::time::timeout(self.timeout, call.call()).await {
            Ok(Ok(result)) => Ok(result._0),
            Ok(Err(error)) => Err(unavailable("isAuthorized", &error)),
            Err(_) => Err(timed_out("isAuthorized")),
        }
    }

    async fn escrow_balance(&self, payer: Address) -> Result<U256, GateRejection> {
        let escrow = IPaymentsEscrow::new(self.escrow, &self.provider);
        let call = escrow.getBalance(payer, self.collector, self.receiver);
        match tokio::time::timeout(self.timeout, call.call()).await {
            Ok(Ok(result)) => Ok(result._0),
            Ok(Err(error)) => Err(unavailable("getBalance", &error)),
            Err(_) => Err(timed_out("getBalance")),
        }
    }
}

fn unavailable(call: &str, error: &dyn std::fmt::Display) -> GateRejection {
    tracing::warn!(%call, %error, "payment gate RPC failed; refusing the request");
    GateRejection::new(
        StatusCode::SERVICE_UNAVAILABLE,
        "payment verification is temporarily unavailable",
    )
}

fn timed_out(call: &str) -> GateRejection {
    tracing::warn!(%call, "payment gate RPC timed out; refusing the request");
    GateRejection::new(
        StatusCode::SERVICE_UNAVAILABLE,
        "payment verification timed out",
    )
}

#[async_trait::async_trait]
impl RequestGate for EscrowGate {
    async fn check(&self, validated: &ValidatedReceipt, _path: &str) -> GateResult {
        let payer = validated.payer;
        let signer = validated.signer;
        let required = self.required_balance(validated.receipt.value);

        if let Some(verdict) = self.cached(payer, signer) {
            return if verdict.balance >= required {
                Ok(())
            } else {
                Err(insufficient_escrow(verdict.balance, required))
            };
        }

        // The payer is read from receipt metadata and is therefore whatever the
        // signer claimed. This is the check that binds the two together, and it
        // is exactly the check GraphTallyCollector will apply at collect time.
        if !self.is_authorized(payer, signer).await? {
            tracing::debug!(%payer, %signer, "receipt signer is not authorized by the named payer");
            return Err(GateRejection::payment_required(format!(
                "receipt signer {signer} is not an authorized signer for payer {payer}"
            )));
        }

        let balance = self.escrow_balance(payer).await?;
        let balance = u128::try_from(balance).unwrap_or(u128::MAX);
        if balance < required {
            return Err(insufficient_escrow(balance, required));
        }

        self.remember(payer, signer, balance);
        Ok(())
    }
}

fn insufficient_escrow(balance: u128, required: u128) -> GateRejection {
    GateRejection::payment_required(format!(
        "escrow balance {balance} is below the {required} required for this request"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn core_config(service_provider: &str, collector: &str) -> Config {
        toml::from_str(&format!(
            r#"
            [server]
            host = "127.0.0.1"
            port = 8090
            [indexer]
            service_provider_address = "{service_provider}"
            operator_private_key = "0x01"
            [tap]
            data_service_address = "0x0000000000000000000000000000000000000002"
            authorized_senders = []
            eip712_verifying_contract = "{collector}"
            [backend]
            upstream_url = "http://127.0.0.1:8110/horizon"
            [database]
            url = "postgres://x/y"
            "#
        ))
        .unwrap()
    }

    fn gate(min_escrow_balance: u128) -> EscrowGate {
        let config = GateConfig {
            rpc_url: "http://127.0.0.1:8545".to_owned(),
            payments_escrow: Address::repeat_byte(0xEE),
            min_escrow_balance,
            cache_ttl_secs: 30,
            rpc_timeout_secs: 5,
        };
        EscrowGate::new(
            &config,
            &core_config(
                "0x0000000000000000000000000000000000000001",
                horizon_core::addresses::GRAPH_TALLY_COLLECTOR,
            ),
        )
        .unwrap()
    }

    #[test]
    fn required_balance_is_the_receipt_value_plus_the_floor() {
        assert_eq!(
            gate(0).required_balance(4_000_000_000_000),
            4_000_000_000_000
        );
        assert_eq!(
            gate(1_000).required_balance(4_000_000_000_000),
            4_000_000_001_000
        );
        // A receipt claiming an absurd value must not wrap the requirement to zero.
        assert_eq!(gate(1_000).required_balance(u128::MAX), u128::MAX);
    }

    #[test]
    fn the_gate_binds_itself_to_the_configured_collector_and_provider() {
        let gate = gate(0);
        assert_eq!(
            gate.collector,
            horizon_core::addresses::GRAPH_TALLY_COLLECTOR
                .parse::<Address>()
                .unwrap(),
            "the collector defaults to the Arbitrum One GraphTallyCollector"
        );
        assert_eq!(
            gate.receiver,
            "0x0000000000000000000000000000000000000001"
                .parse::<Address>()
                .unwrap()
        );
    }

    #[tokio::test]
    async fn a_fresh_verdict_short_circuits_and_is_still_compared_against_the_requirement() {
        let gate = gate(0);
        let payer = Address::repeat_byte(0xAA);
        let signer = Address::repeat_byte(0xBB);
        gate.remember(payer, signer, 10_000);

        let cached = gate.cached(payer, signer).expect("verdict should be fresh");
        assert_eq!(cached.balance, 10_000);
        // Cached does not mean admitted: a costlier route still has to clear it.
        assert!(cached.balance < gate.required_balance(20_000));
    }

    #[test]
    fn an_unknown_pair_has_no_cached_verdict() {
        let gate = gate(0);
        assert!(
            gate.cached(Address::repeat_byte(1), Address::repeat_byte(2))
                .is_none()
        );
    }

    #[test]
    fn the_verdict_cache_is_bounded() {
        let gate = gate(0);
        for i in 0..4_200u32 {
            let mut signer = [0u8; 20];
            signer[..4].copy_from_slice(&i.to_be_bytes());
            gate.remember(Address::repeat_byte(0xAA), Address::from(signer), 1);
        }
        assert!(gate.cache.lock().unwrap().len() <= 4096);
    }

    fn validated(payer: Address, signer: Address, value: u128) -> ValidatedReceipt {
        ValidatedReceipt {
            receipt: horizon_core::tap::Receipt {
                data_service: Address::repeat_byte(0x02),
                service_provider: payer,
                timestamp_ns: 1,
                nonce: 1,
                value,
                metadata: Default::default(),
            },
            signer,
            payer,
            signature: "0x00".to_owned(),
        }
    }

    #[tokio::test]
    async fn an_unreachable_rpc_refuses_the_request_rather_than_admitting_it() {
        // Nothing listens on port 1. The gate must decline, not shrug and forward.
        let config = GateConfig {
            rpc_url: "http://127.0.0.1:1".to_owned(),
            payments_escrow: Address::repeat_byte(0xEE),
            min_escrow_balance: 0,
            cache_ttl_secs: 30,
            rpc_timeout_secs: 5,
        };
        let gate = EscrowGate::new(
            &config,
            &core_config(
                "0x0000000000000000000000000000000000000001",
                horizon_core::addresses::GRAPH_TALLY_COLLECTOR,
            ),
        )
        .unwrap();

        let rejection = gate
            .check(
                &validated(Address::repeat_byte(0xAA), Address::repeat_byte(0xBB), 1),
                "/v1/nests/x/q/top_indexers",
            )
            .await
            .unwrap_err();
        assert_eq!(rejection.status, StatusCode::SERVICE_UNAVAILABLE);
    }

    // -------------------------------------------------------------------------
    // Live Arbitrum One
    //
    // Ignored by default: needs network and an RPC endpoint. Run with
    //
    //   NUTHATCH_TEST_RPC_URL=https://arb-mainnet.example/v2/KEY \
    //     cargo test --locked -- --ignored --nocapture
    //
    // The assertions are pinned to facts the beta established on-chain, so this
    // fails loudly if the ABI, the addresses, or the escrow keying are wrong —
    // which is the part no offline test can reach.
    // -------------------------------------------------------------------------

    /// The beta provider, payer and RAV signer, all one address on Arbitrum One.
    const BETA_PROVIDER: &str = "0x02526499Ae2879A94090267017C3816f733825Bb";
    const ARBITRUM_ONE_ESCROW: &str = "0xf6Fcc27aAf1fcD8B254498c9794451d82afC673E";
    const ARBITRUM_ONE_COLLECTOR: &str = "0x8f69F5C07477Ac46FBc491B1E6D91E2bb0111A9e";

    fn live_gate(rpc_url: String, min_escrow_balance: u128) -> EscrowGate {
        let config = GateConfig {
            rpc_url,
            payments_escrow: ARBITRUM_ONE_ESCROW.parse().unwrap(),
            min_escrow_balance,
            cache_ttl_secs: 30,
            rpc_timeout_secs: 15,
        };
        EscrowGate::new(&config, &core_config(BETA_PROVIDER, ARBITRUM_ONE_COLLECTOR)).unwrap()
    }

    #[tokio::test]
    #[ignore = "needs NUTHATCH_TEST_RPC_URL pointing at an Arbitrum One endpoint"]
    async fn the_gate_reads_real_arbitrum_one_state() {
        let rpc_url = std::env::var("NUTHATCH_TEST_RPC_URL")
            .expect("set NUTHATCH_TEST_RPC_URL to an Arbitrum One RPC endpoint");
        let gate = live_gate(rpc_url, 0);
        let provider: Address = BETA_PROVIDER.parse().unwrap();
        let stranger = Address::repeat_byte(0xAB);

        // The beta provider authorised itself as a RAV signer; that is what made
        // the mainnet collect() proof redeemable.
        assert!(
            gate.is_authorized(provider, provider).await.unwrap(),
            "the beta provider should be an authorized signer for itself"
        );

        // An address that has authorised nobody. This is the check that binds a
        // receipt's self-declared payer to whoever actually signed it.
        assert!(
            !gate.is_authorized(stranger, stranger).await.unwrap(),
            "an unrelated address must not be an authorized signer"
        );
        assert!(
            !gate.is_authorized(provider, stranger).await.unwrap(),
            "a stranger must not pass as a signer for the beta provider"
        );

        // The proof drained the escrow: "afterwards the payment escrow balance
        // was zero". If the escrow tuple were keyed wrongly this would not be
        // readable at all, let alone correct.
        assert_eq!(
            gate.escrow_balance(provider).await.unwrap(),
            U256::ZERO,
            "the beta escrow was emptied by the mainnet collect() proof"
        );
    }

    #[tokio::test]
    #[ignore = "needs NUTHATCH_TEST_RPC_URL pointing at an Arbitrum One endpoint"]
    async fn an_empty_escrow_is_refused_end_to_end() {
        let rpc_url = std::env::var("NUTHATCH_TEST_RPC_URL")
            .expect("set NUTHATCH_TEST_RPC_URL to an Arbitrum One RPC endpoint");
        let gate = live_gate(rpc_url.clone(), 0);
        let provider: Address = BETA_PROVIDER.parse().unwrap();

        // An authorised signer with a drained escrow: past the authorisation
        // check, refused on balance. One compute unit at the published rate.
        let rejection = gate
            .check(
                &validated(provider, provider, 4_000_000_000_000),
                "/v1/nests/x/q/top_indexers",
            )
            .await
            .unwrap_err();
        assert_eq!(rejection.status, StatusCode::PAYMENT_REQUIRED);
        assert!(
            rejection.message.contains("escrow balance 0"),
            "expected an escrow rejection, got: {}",
            rejection.message
        );

        // An unauthorised signer is refused earlier, and for a different reason.
        let rejection = live_gate(rpc_url, 0)
            .check(
                &validated(provider, Address::repeat_byte(0xAB), 1),
                "/v1/nests/x/q/top_indexers",
            )
            .await
            .unwrap_err();
        assert_eq!(rejection.status, StatusCode::PAYMENT_REQUIRED);
        assert!(
            rejection.message.contains("not an authorized signer"),
            "expected a signer rejection, got: {}",
            rejection.message
        );
    }
}
