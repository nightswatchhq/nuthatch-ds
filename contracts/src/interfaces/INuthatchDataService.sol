// SPDX-License-Identifier: Apache-2.0
pragma solidity ^0.8.27;

/// @title INuthatchDataService
/// @notice Interface for the Nuthatch Data Service on The Graph Protocol's Horizon framework.
///
/// Paid access to reproducible self-hosted Nuthatch indexed datasets on Graph Horizon.
///
/// Provider lifecycle:
///   provision → register → startService (per NID and query mode) → [collect]* → stopService → deregister
interface INuthatchDataService {
    // -------------------------------------------------------------------------
    // Types
    // -------------------------------------------------------------------------

    /// @notice The query surface a provider exposes for a nest.
    enum QueryMode {
        NAMED,  // 0 — Author-sanctioned named queries
        SQL     // 1 — Arbitrary analytical SQL
    }

    /// @notice An active or historical offering of one reproducible nest.
    struct NestOffering {
        bytes32 nid;
        QueryMode mode;
        string endpoint;
        bool active;
    }

    // -------------------------------------------------------------------------
    // Events
    // -------------------------------------------------------------------------

    event ProviderRegistered(address indexed provider, string endpoint, string geoHash);
    event ProviderDeregistered(address indexed provider);
    event PaymentsDestinationSet(address indexed provider, address indexed destination);
    event OfferingStarted(address indexed provider, bytes32 indexed nid, QueryMode mode, string endpoint);
    event OfferingStopped(address indexed provider, bytes32 indexed nid, QueryMode mode);
    event MinThawingPeriodSet(uint64 period);
    event FeesBurned(address indexed provider, uint256 amount);
    event FeesWithdrawn(address indexed to, uint256 amount);

    // -------------------------------------------------------------------------
    // Errors
    // -------------------------------------------------------------------------

    error ProviderAlreadyRegistered(address provider);
    error ProviderNotRegistered(address provider);
    error ActiveServicesExist(address provider);
    error OfferingNotFound(address provider, bytes32 nid, QueryMode mode);
    error InvalidNid();
    error InvalidServiceProvider(address expected, address actual);
    error InvalidPaymentType();
    error ThawingPeriodTooShort(uint64 required, uint64 actual);

    // -------------------------------------------------------------------------
    // Provider operations
    // -------------------------------------------------------------------------

    /// @notice Update the address that receives collected GRT fees.
    function setPaymentsDestination(address destination) external;

    // -------------------------------------------------------------------------
    // Governance
    // -------------------------------------------------------------------------

    /// @notice Update the minimum thawing period (lower-bounded by MIN_THAWING_PERIOD).
    function setMinThawingPeriod(uint64 period) external;

    /// @notice Withdraw accumulated data-service revenue to `to`.
    function withdrawFees(address to, uint256 amount) external;

    // -------------------------------------------------------------------------
    // Views
    // -------------------------------------------------------------------------

    function isRegistered(address provider) external view returns (bool);

    function getServiceRegistrations(address provider)
        external
        view
        returns (NestOffering[] memory);

    /// @notice Stable identifier for an offering under a provider.
    function offeringKey(bytes32 nid, QueryMode mode) external pure returns (bytes32);

    function activeServiceCount(address provider) external view returns (uint256);

    function paymentsDestination(address provider) external view returns (address);

    function minThawingPeriod() external view returns (uint64);
}
