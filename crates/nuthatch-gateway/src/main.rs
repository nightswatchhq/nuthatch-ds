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
}
