//! Per-endpoint pricing for Nuthatch's deliberately small public surface.

/// GRT wei per compute unit.
pub const BASE_PRICE_PER_CU: u128 = 4_000_000_000_000;

/// Compute-unit cost for a public Nuthatch route.
///
/// `/schema` and `/queries` are handled as free discovery routes. They never
/// reach this policy. Unknown paths cost zero because the router does not expose
/// them, rather than because they are a bargain.
pub fn cu_cost(path: &str) -> u32 {
    let path = path.trim_end_matches('/');
    if path.contains("/sql") {
        20
    } else if path.contains("/table/") {
        2
    } else if path.contains("/q/") {
        1
    } else {
        0
    }
}

pub fn min_receipt_value(path: &str) -> u128 {
    cu_cost(path) as u128 * BASE_PRICE_PER_CU
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_surface_is_priced_as_intended() {
        assert_eq!(cu_cost("/v1/nests/nid/schema"), 0);
        assert_eq!(cu_cost("/v1/nests/nid/q/top_indexers"), 1);
        assert_eq!(cu_cost("/v1/nests/nid/table/allocations"), 2);
        assert_eq!(cu_cost("/v1/nests/nid/sql"), 20);
    }
}
