//! Nuthatch Data Service gateway.
//!
//! A narrow, NID-addressed public surface in front of a private Nuthatch runtime.
//! This is deliberately not a transparent proxy, and it does not compose one by
//! accident either: the router below names five routes and answers everything
//! else with 404. Nuthatch administration, metrics, internal metadata and its
//! general routing surface stay on the private side of the gateway, along with
//! every other attic door.

use std::{net::SocketAddr, sync::Arc};

use axum::{
    Router,
    body::Body,
    extract::{Path, State},
    http::{Request, StatusCode},
    response::Response,
    routing::get,
};
use horizon_core::{AppState, Config, SharedPricing, pricing::FnPricing};
use tower_governor::{GovernorLayer, governor::GovernorConfigBuilder};

mod config;
mod escrow;
mod pricing;

type GatewayResult = Result<Response<Body>, (StatusCode, String)>;

/// Upstream responses are buffered before being relayed. Nuthatch's named
/// queries take a caller-supplied `LIMIT`, and the gateway runs under a 512 MiB
/// systemd cap, so one cheap query must not be able to spend all of it.
const MAX_UPSTREAM_RESPONSE_BYTES: usize = 8 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum QueryMode {
    Named,
    Sql,
}

impl QueryMode {
    fn from_env() -> anyhow::Result<Self> {
        Self::parse(&std::env::var("NUTHATCH_QUERY_MODE").unwrap_or_else(|_| "NAMED".to_owned()))
    }

    fn parse(value: &str) -> anyhow::Result<Self> {
        match value.to_ascii_uppercase().as_str() {
            "NAMED" => Ok(Self::Named),
            "SQL" => Ok(Self::Sql),
            value => anyhow::bail!("NUTHATCH_QUERY_MODE must be NAMED or SQL, got {value}"),
        }
    }
}

/// The NID this gateway serves. It is intentionally configuration, not caller input:
/// an on-chain provider advertises the same NID and endpoint as its offering.
fn configured_nid() -> anyhow::Result<String> {
    parse_nid(&std::env::var("NUTHATCH_NID")?)
}

fn parse_nid(nid: &str) -> anyhow::Result<String> {
    if nid.len() != 64 || !nid.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        anyhow::bail!("NUTHATCH_NID must be a 64-character hexadecimal NID")
    }
    Ok(nid.to_ascii_lowercase())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenvy::dotenv().ok();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "nuthatch_gateway=info,horizon_core=info".into()),
        )
        .init();

    let nid = configured_nid()?;
    let mode = QueryMode::from_env()?;
    let core = Config::load()?;
    let nuthatch = config::NuthatchConfig::load()?;
    config::validate(&core, &nuthatch)?;

    let policy: SharedPricing = Arc::new(FnPricing(|path: &str| pricing::min_receipt_value(path)));
    let mut state = horizon_core::build_state_with(Arc::new(core), policy).await?;

    match &nuthatch.gate {
        Some(gate) => {
            tracing::info!(
                escrow = %gate.payments_escrow,
                min_escrow_balance = gate.min_escrow_balance,
                cache_ttl_secs = gate.cache_ttl_secs,
                "on-chain payment gate enabled"
            );
            let gate = escrow::EscrowGate::new(gate, &state.config)?;
            state = state.with_gate(Arc::new(gate));
        }
        None => tracing::warn!(
            authorized_senders = state.config.tap.authorized_senders.len(),
            "no [gate] section: paid routes are guarded by the sender allowlist alone, and \
             neither signer authorisation nor escrow balance is checked before serving"
        ),
    }

    horizon_core::spawn_background(&state);

    tracing::info!(
        upstream = %state.config.backend.upstream_url,
        data_service = %state.config.tap.data_service_address,
        %nid,
        ?mode,
        "Nuthatch Data Service gateway starting"
    );

    let addr = format!("{}:{}", state.config.server.host, state.config.server.port);
    let app = app(state, nid, mode);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!(%addr, "nuthatch-gateway listening");
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await?;
    Ok(())
}

/// Compose the served application.
///
/// Note what is *not* here: no catch-all, no fallthrough to a transparent proxy.
/// `horizon_core::run_with` would add one, which would leave every unnamed path
/// proxied straight to Nuthatch, so the gateway assembles its own router and
/// answers anything it does not recognise with 404.
fn app(state: AppState, nid: String, mode: QueryMode) -> Router {
    let rate_limit = &state.config.rate_limit;
    let period_ms = 1_000u64 / u64::from(rate_limit.requests_per_second.max(1));
    let governor = Arc::new(
        GovernorConfigBuilder::default()
            .per_millisecond(period_ms)
            .burst_size(rate_limit.burst_size)
            .finish()
            .expect("invalid [rate_limit] configuration"),
    );
    tracing::info!(
        rps = rate_limit.requests_per_second,
        burst = rate_limit.burst_size,
        "rate limiter configured"
    );

    // The rate limiter covers the public routes and the fallback, so probing for
    // unlisted paths is throttled the same as asking for real ones. /health and
    // /ready stay unthrottled for the supervisor and the load balancer.
    let public = public_routes(nid, mode)
        .fallback(not_found)
        .layer(GovernorLayer::new(governor));

    Router::new()
        .route("/health", get(health))
        .route("/ready", get(ready))
        .merge(public)
        .with_state(state)
}

fn public_routes(nid: String, mode: QueryMode) -> Router<AppState> {
    Router::new()
        .route("/v1/nests/{nid}/schema", get(schema))
        .route("/v1/nests/{nid}/queries", get(queries))
        .route("/v1/nests/{nid}/q/{query}", get(named_query))
        .route("/v1/nests/{nid}/table/{table}", get(table))
        .route("/v1/nests/{nid}/sql", get(sql))
        .layer(axum::Extension(PublicConfig { nid, mode }))
}

async fn not_found() -> (StatusCode, &'static str) {
    (StatusCode::NOT_FOUND, "no such route")
}

async fn health() -> StatusCode {
    StatusCode::OK
}

async fn ready(State(state): State<AppState>) -> StatusCode {
    match state.pool.acquire().await {
        Ok(_) => StatusCode::OK,
        Err(_) => StatusCode::SERVICE_UNAVAILABLE,
    }
}

#[derive(Clone)]
struct PublicConfig {
    nid: String,
    mode: QueryMode,
}

async fn schema(
    State(state): State<AppState>,
    axum::Extension(config): axum::Extension<PublicConfig>,
    Path(nid): Path<String>,
    request: Request<Body>,
) -> GatewayResult {
    forward_discovery(&state, &config, nid, "/schema", request).await
}

async fn queries(
    State(state): State<AppState>,
    axum::Extension(config): axum::Extension<PublicConfig>,
    Path(nid): Path<String>,
    request: Request<Body>,
) -> GatewayResult {
    forward_discovery(&state, &config, nid, "/queries", request).await
}

async fn named_query(
    State(state): State<AppState>,
    axum::Extension(config): axum::Extension<PublicConfig>,
    Path((nid, query)): Path<(String, String)>,
    request: Request<Body>,
) -> GatewayResult {
    let upstream_path = nuthatch_path("q", &query)?;
    forward_paid(&state, &config, nid, &upstream_path, request).await
}

async fn table(
    State(state): State<AppState>,
    axum::Extension(config): axum::Extension<PublicConfig>,
    Path((nid, table)): Path<(String, String)>,
    request: Request<Body>,
) -> GatewayResult {
    let upstream_path = nuthatch_path("table", &table)?;
    forward_paid(&state, &config, nid, &upstream_path, request).await
}

async fn sql(
    State(state): State<AppState>,
    axum::Extension(config): axum::Extension<PublicConfig>,
    Path(nid): Path<String>,
    request: Request<Body>,
) -> GatewayResult {
    if config.mode != QueryMode::Sql {
        return Err((
            StatusCode::NOT_FOUND,
            "SQL is not offered by this endpoint".to_owned(),
        ));
    }
    forward_paid(&state, &config, nid, "/sql", request).await
}

async fn forward_discovery(
    state: &AppState,
    config: &PublicConfig,
    nid: String,
    upstream_path: &str,
    request: Request<Body>,
) -> GatewayResult {
    check_nid(config, &nid)?;
    forward(state, upstream_path, request).await
}

async fn forward_paid(
    state: &AppState,
    config: &PublicConfig,
    nid: String,
    upstream_path: &str,
    request: Request<Body>,
) -> GatewayResult {
    check_nid(config, &nid)?;
    let receipt = request
        .headers()
        .get("tap-receipt")
        .ok_or_else(|| {
            (
                StatusCode::PAYMENT_REQUIRED,
                "TAP-Receipt header required".to_owned(),
            )
        })?
        .to_str()
        .map_err(|_| {
            (
                StatusCode::BAD_REQUEST,
                "TAP-Receipt is not valid UTF-8".to_owned(),
            )
        })?;
    horizon_core::proxy::gate_request(state, receipt, request.uri().path()).await?;
    forward(state, upstream_path, request).await
}

fn check_nid(config: &PublicConfig, nid: &str) -> Result<(), (StatusCode, String)> {
    if nid.eq_ignore_ascii_case(&config.nid) {
        Ok(())
    } else {
        Err((
            StatusCode::NOT_FOUND,
            "NID is not served by this endpoint".to_owned(),
        ))
    }
}

/// Build one of Nuthatch's identifier-addressed public paths.
///
/// Query and table names become part of the upstream URL, so accept only the
/// grammar Nuthatch uses for identifiers. In particular, reject percent signs,
/// dots and slashes rather than allowing a caller to smuggle another upstream
/// path through an otherwise narrow gateway.
fn nuthatch_path(kind: &str, identifier: &str) -> Result<String, (StatusCode, String)> {
    if identifier.is_empty()
        || !identifier
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
    {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("invalid Nuthatch {kind} identifier"),
        ));
    }
    Ok(format!("/{kind}/{identifier}"))
}

async fn forward(state: &AppState, upstream_path: &str, request: Request<Body>) -> GatewayResult {
    let query = request
        .uri()
        .query()
        .map(|query| format!("?{query}"))
        .unwrap_or_default();
    let url = format!(
        "{}{}{}",
        state.config.backend.upstream_url.trim_end_matches('/'),
        upstream_path,
        query
    );
    let mut builder = state.http_client.get(url);
    if let Some(accept) = request.headers().get("accept") {
        builder = builder.header("accept", accept);
    }
    let mut response = builder
        .send()
        .await
        .map_err(|error| (StatusCode::BAD_GATEWAY, error.to_string()))?;
    let status =
        StatusCode::from_u16(response.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("application/octet-stream")
        .to_owned();

    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| (StatusCode::BAD_GATEWAY, error.to_string()))?
    {
        if body.len() + chunk.len() > MAX_UPSTREAM_RESPONSE_BYTES {
            tracing::warn!(
                path = upstream_path,
                limit = MAX_UPSTREAM_RESPONSE_BYTES,
                "upstream response exceeded the relay limit"
            );
            return Err((
                StatusCode::BAD_GATEWAY,
                format!("upstream response exceeds {MAX_UPSTREAM_RESPONSE_BYTES} bytes"),
            ));
        }
        body.extend_from_slice(&chunk);
    }

    Ok(Response::builder()
        .status(status)
        .header("content-type", content_type)
        .body(Body::from(body))
        .unwrap())
}

#[cfg(test)]
mod tests {
    use super::*;

    const NID: &str = "36d3c71446a56cdb5b90536d3f5f77351b1d92efcca94bc2fd41b1c368e69410";

    #[test]
    fn parses_a_canonical_nid() {
        assert_eq!(parse_nid(&NID.to_ascii_uppercase()).unwrap(), NID);
    }

    #[test]
    fn rejects_invalid_nids() {
        for nid in [
            "",
            "0x36d3c71446a56cdb5b90536d3f5f77351b1d92efcca94bc2fd41b1c368e69410",
            "xyz",
        ] {
            assert!(parse_nid(nid).is_err(), "{nid} should be rejected");
        }
    }

    #[test]
    fn parses_query_modes_case_insensitively() {
        assert_eq!(QueryMode::parse("named").unwrap(), QueryMode::Named);
        assert_eq!(QueryMode::parse("SQL").unwrap(), QueryMode::Sql);
        assert!(QueryMode::parse("everything").is_err());
    }

    #[test]
    fn only_allows_safe_upstream_identifiers() {
        assert_eq!(
            nuthatch_path("q", "top_indexers").unwrap(),
            "/q/top_indexers"
        );
        assert_eq!(
            nuthatch_path("table", "erc20-transfers").unwrap(),
            "/table/erc20-transfers"
        );
        for identifier in ["", ".", "..", "a/b", "%2f_admin", "query?sql=select"] {
            let error = nuthatch_path("q", identifier).unwrap_err();
            assert_eq!(
                error.0,
                StatusCode::BAD_REQUEST,
                "{identifier} should be rejected"
            );
        }
    }

    #[test]
    fn only_the_configured_nid_is_served() {
        let config = PublicConfig {
            nid: NID.to_owned(),
            mode: QueryMode::Named,
        };
        assert!(check_nid(&config, &NID.to_ascii_uppercase()).is_ok());
        assert_eq!(
            check_nid(&config, "00").unwrap_err().0,
            StatusCode::NOT_FOUND
        );
    }

    // -------------------------------------------------------------------------
    // Router shape
    //
    // These exercise the composed application rather than the handlers, because
    // the interesting failure is a *routing* one: a catch-all merged in
    // alongside the public routes turns a narrow gateway into a transparent
    // proxy without changing a single handler.
    // -------------------------------------------------------------------------

    use axum::extract::ConnectInfo;
    use tower::ServiceExt;

    const CONFIG: &str = r#"
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
        url = "postgres://nuthatch:pw@127.0.0.1:1/nuthatch_gateway"
    "#;

    /// An `AppState` that never reaches the network. Every assertion below is
    /// decided by the router or a handler guard, well before the pool or the
    /// upstream is touched.
    fn test_state(extra: &str) -> AppState {
        let config: Config = toml::from_str(&format!("{CONFIG}\n{extra}")).unwrap();
        let upstream = config.backend.upstream_url.clone();
        AppState {
            config: Arc::new(config),
            pool: sqlx::postgres::PgPoolOptions::new()
                // Nothing listens on port 1, and we would rather find that out
                // promptly than sit through the 30 second default.
                .acquire_timeout(std::time::Duration::from_millis(200))
                .connect_lazy("postgres://nuthatch:pw@127.0.0.1:1/nuthatch_gateway")
                .unwrap(),
            http_client: reqwest::Client::new(),
            domain_sep: Default::default(),
            pricing: Arc::new(FnPricing(|path: &str| pricing::min_receipt_value(path))),
            gate: horizon_core::gate::allow_all(),
            backend: Arc::new(horizon_core::backend::SingleBackend(upstream)),
        }
    }

    async fn get(app: Router, uri: &str) -> StatusCode {
        let mut request = Request::builder().uri(uri).body(Body::empty()).unwrap();
        // Mirrors into_make_service_with_connect_info: the rate limiter keys on it.
        request.extensions_mut().insert(ConnectInfo(
            "127.0.0.1:40000".parse::<SocketAddr>().unwrap(),
        ));
        app.oneshot(request).await.unwrap().status()
    }

    fn named_mode_app() -> Router {
        app(test_state(""), NID.to_owned(), QueryMode::Named)
    }

    #[tokio::test]
    async fn unlisted_paths_are_refused_rather_than_proxied() {
        for path in [
            "/",
            "/sql",
            "/sql?q=SELECT%201",
            "/explain",
            "/admin",
            "/metrics",
            "/tables",
            "/q/top_indexers",
            "/table/allocations",
            "/schema",
            "/../admin",
            "/v1/nests",
            &format!("/v1/nests/{NID}"),
            &format!("/v1/nests/{NID}/explain"),
        ] {
            assert_eq!(
                get(named_mode_app(), path).await,
                StatusCode::NOT_FOUND,
                "{path} must not reach the upstream"
            );
        }
    }

    #[tokio::test]
    async fn paid_routes_demand_a_receipt() {
        for path in [
            format!("/v1/nests/{NID}/q/top_indexers"),
            format!("/v1/nests/{NID}/table/allocations"),
        ] {
            assert_eq!(
                get(named_mode_app(), &path).await,
                StatusCode::PAYMENT_REQUIRED,
                "{path} must be gated"
            );
        }
    }

    #[tokio::test]
    async fn sql_is_absent_in_named_mode_and_gated_in_sql_mode() {
        let path = format!("/v1/nests/{NID}/sql?q=SELECT%201");
        assert_eq!(get(named_mode_app(), &path).await, StatusCode::NOT_FOUND);

        let sql_mode = app(test_state(""), NID.to_owned(), QueryMode::Sql);
        assert_eq!(get(sql_mode, &path).await, StatusCode::PAYMENT_REQUIRED);
    }

    #[tokio::test]
    async fn another_nid_is_not_served_even_on_a_free_route() {
        let other = "0".repeat(64);
        assert_eq!(
            get(named_mode_app(), &format!("/v1/nests/{other}/schema")).await,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            get(
                named_mode_app(),
                &format!("/v1/nests/{other}/q/top_indexers")
            )
            .await,
            StatusCode::NOT_FOUND
        );
    }

    #[tokio::test]
    async fn malformed_identifiers_are_rejected_before_the_upstream() {
        assert_eq!(
            get(
                named_mode_app(),
                &format!("/v1/nests/{NID}/q/top%2Findexers")
            )
            .await,
            StatusCode::BAD_REQUEST
        );
    }

    #[tokio::test]
    async fn health_needs_no_database_and_ready_does() {
        assert_eq!(get(named_mode_app(), "/health").await, StatusCode::OK);
        assert_eq!(
            get(named_mode_app(), "/ready").await,
            StatusCode::SERVICE_UNAVAILABLE
        );
    }

    #[tokio::test]
    async fn the_rate_limiter_covers_public_routes_and_the_fallback() {
        let throttled = app(
            test_state("[rate_limit]\nrequests_per_second = 1\nburst_size = 1"),
            NID.to_owned(),
            QueryMode::Named,
        );
        assert_eq!(
            get(throttled.clone(), "/admin").await,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            get(throttled, "/admin").await,
            StatusCode::TOO_MANY_REQUESTS,
            "probing for unlisted paths must be throttled too"
        );
    }

    #[tokio::test]
    async fn health_is_never_throttled() {
        let throttled = app(
            test_state("[rate_limit]\nrequests_per_second = 1\nburst_size = 1"),
            NID.to_owned(),
            QueryMode::Named,
        );
        for _ in 0..5 {
            assert_eq!(get(throttled.clone(), "/health").await, StatusCode::OK);
        }
    }
}
