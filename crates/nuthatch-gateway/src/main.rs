//! Nuthatch Data Service gateway.
//!
//! This is intentionally not a transparent proxy. It publishes a narrow NID-addressed
//! surface and leaves Nuthatch administration, metrics, arbitrary internal routes, and
//! every other attic door on the private side of the gateway.

use std::sync::Arc;

use axum::{
    Router,
    body::Body,
    extract::{Path, State},
    http::{Request, StatusCode},
    response::Response,
    routing::get,
};
use horizon_core::{AppState, Config, SharedPricing, pricing::FnPricing};

mod pricing;

type GatewayResult = Result<Response<Body>, (StatusCode, String)>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum QueryMode {
    Named,
    Sql,
}

impl QueryMode {
    fn from_env() -> anyhow::Result<Self> {
        match std::env::var("NUTHATCH_QUERY_MODE")
            .unwrap_or_else(|_| "NAMED".to_owned())
            .to_ascii_uppercase()
            .as_str()
        {
            "NAMED" => Ok(Self::Named),
            "SQL" => Ok(Self::Sql),
            value => anyhow::bail!("NUTHATCH_QUERY_MODE must be NAMED or SQL, got {value}"),
        }
    }
}

/// The NID this gateway serves. It is intentionally configuration, not caller input:
/// an on-chain provider advertises the same NID and endpoint as its offering.
fn configured_nid() -> anyhow::Result<String> {
    let nid = std::env::var("NUTHATCH_NID")?;
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
    let config = Config::load()?;
    let policy: SharedPricing = Arc::new(FnPricing(|path: &str| pricing::min_receipt_value(path)));
    let routes = public_routes(nid.clone(), mode);

    tracing::info!(
        upstream = %config.backend.upstream_url,
        data_service = %config.tap.data_service_address,
        %nid,
        ?mode,
        "Nuthatch Data Service gateway starting"
    );

    horizon_core::run_with(config, policy, routes).await
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
    forward_paid(&state, &config, nid, &format!("/q/{query}"), request).await
}

async fn table(
    State(state): State<AppState>,
    axum::Extension(config): axum::Extension<PublicConfig>,
    Path((nid, table)): Path<(String, String)>,
    request: Request<Body>,
) -> GatewayResult {
    forward_paid(&state, &config, nid, &format!("/table/{table}"), request).await
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
    let response = builder
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
    let bytes = response
        .bytes()
        .await
        .map_err(|error| (StatusCode::BAD_GATEWAY, error.to_string()))?;
    Ok(Response::builder()
        .status(status)
        .header("content-type", content_type)
        .body(Body::from(bytes))
        .unwrap())
}
