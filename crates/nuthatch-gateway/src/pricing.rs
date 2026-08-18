//! Per-endpoint pricing for Nuthatch's deliberately small public surface.
//!
//! Pricing matches the route shape, not a substring of the path. A named query
//! called `sql_top_indexers` is a 1 CU named query, and a path this gateway does
//! not serve has no price at all rather than a free one.

/// GRT wei per compute unit.
pub const BASE_PRICE_PER_CU: u128 = 4_000_000_000_000;

/// Compute-unit cost of a public Nuthatch route, or `None` if the path is not
/// one of the paid routes.
///
/// `/schema` and `/queries` are free discovery and deliberately return `None`:
/// they never reach the receipt gate, so they never consult this policy.
pub fn cu_cost(path: &str) -> Option<u32> {
    let mut segments = path.trim_end_matches('/').split('/');

    // Leading empty segment, then the fixed `/v1/nests/{nid}` prefix.
    if segments.next() != Some("")
        || segments.next() != Some("v1")
        || segments.next() != Some("nests")
    {
        return None;
    }
    // The NID is matched against configuration by the handler, not priced here.
    segments.next()?;

    let cost = match (segments.next()?, segments.next()) {
        ("q", Some(identifier)) if !identifier.is_empty() => 1,
        ("table", Some(identifier)) if !identifier.is_empty() => 2,
        ("sql", None) => 20,
        _ => return None,
    };

    // Anything deeper than the route we recognised is not that route.
    if segments.next().is_some() {
        return None;
    }
    Some(cost)
}

/// Minimum receipt value, in GRT wei, that admits a request to `path`.
///
/// An unrecognised path costs `u128::MAX`, which no receipt can satisfy. The
/// router should have refused it long before this point; if a paid route is ever
/// added without a price, it fails closed rather than free.
pub fn min_receipt_value(path: &str) -> u128 {
    match cu_cost(path) {
        Some(cu) => u128::from(cu) * BASE_PRICE_PER_CU,
        None => u128::MAX,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NID: &str = "36d3c71446a56cdb5b90536d3f5f77351b1d92efcca94bc2fd41b1c368e69410";

    #[test]
    fn public_surface_is_priced_as_intended() {
        assert_eq!(cu_cost(&format!("/v1/nests/{NID}/q/top_indexers")), Some(1));
        assert_eq!(
            cu_cost(&format!("/v1/nests/{NID}/table/allocations")),
            Some(2)
        );
        assert_eq!(cu_cost(&format!("/v1/nests/{NID}/sql")), Some(20));
    }

    #[test]
    fn free_discovery_routes_are_not_priced() {
        assert_eq!(cu_cost(&format!("/v1/nests/{NID}/schema")), None);
        assert_eq!(cu_cost(&format!("/v1/nests/{NID}/queries")), None);
    }

    #[test]
    fn a_named_query_is_priced_by_route_not_by_its_name() {
        // The old substring policy charged 20 CU for this, because the name
        // happens to contain "sql" after a slash.
        assert_eq!(
            cu_cost(&format!("/v1/nests/{NID}/q/sql_top_indexers")),
            Some(1)
        );
        assert_eq!(
            cu_cost(&format!("/v1/nests/{NID}/table/sql_audit")),
            Some(2)
        );
        assert_eq!(cu_cost(&format!("/v1/nests/{NID}/q/table")), Some(1));
    }

    #[test]
    fn unknown_paths_have_no_price_at_all() {
        for path in [
            "/sql",
            "/explain",
            "/admin",
            "/metrics",
            "/v1/nests",
            &format!("/v1/nests/{NID}"),
            &format!("/v1/nests/{NID}/explain"),
            &format!("/v1/nests/{NID}/q"),
            &format!("/v1/nests/{NID}/q/a/b"),
            &format!("/v1/nests/{NID}/sql/inject"),
            &format!("/v2/nests/{NID}/q/top_indexers"),
        ] {
            assert_eq!(cu_cost(path), None, "{path} should not be a priced route");
            assert_eq!(
                min_receipt_value(path),
                u128::MAX,
                "{path} should be unpayable, not free"
            );
        }
    }

    #[test]
    fn prices_are_compute_units_times_the_base_rate() {
        assert_eq!(
            min_receipt_value(&format!("/v1/nests/{NID}/table/allocations")),
            2 * BASE_PRICE_PER_CU
        );
    }
}
