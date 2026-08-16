// SPDX-License-Identifier: Apache-2.0
pragma solidity ^0.8.27;

import {OwnableUpgradeable} from "@openzeppelin/contracts-upgradeable/access/OwnableUpgradeable.sol";
import {UUPSUpgradeable} from "@openzeppelin/contracts-upgradeable/proxy/utils/UUPSUpgradeable.sol";

import {DataService} from "@graphprotocol/horizon/data-service/DataService.sol";
import {DataServiceFees} from "@graphprotocol/horizon/data-service/extensions/DataServiceFees.sol";
import {
    DataServicePausableUpgradeable
} from "@graphprotocol/horizon/data-service/extensions/DataServicePausableUpgradeable.sol";
import {IGraphPayments} from "@graphprotocol/interfaces/contracts/horizon/IGraphPayments.sol";
import {IGraphTallyCollector} from "@graphprotocol/interfaces/contracts/horizon/IGraphTallyCollector.sol";

import {INuthatchDataService} from "./interfaces/INuthatchDataService.sol";

/// @title NuthatchDataService
/// @notice Nuthatch Data Service on The Graph Protocol's Horizon framework.
///
/// Paid access to reproducible self-hosted Nuthatch indexed datasets on Graph Horizon.
///
/// @dev DISCLAIMER: experimental community project. Not affiliated with or endorsed by
///      The Graph Foundation or Edge & Node.
///
/// @dev Inherits DataService (provision utilities, GraphDirectory), DataServiceFees
///      (stake-backed fee locking), DataServicePausableUpgradeable (emergency stop).
///      Deployed as a UUPS upgradeable proxy.
contract NuthatchDataService is
    OwnableUpgradeable,
    UUPSUpgradeable,
    DataService,
    DataServiceFees,
    DataServicePausableUpgradeable,
    INuthatchDataService
{
    // -------------------------------------------------------------------------
    // Constants
    // -------------------------------------------------------------------------

    /// @notice Minimum GRT provision per registered provider.
    uint256 public constant MIN_PROVISION = 555e18;

    /// @notice Fraction of collected fees burned (PPM: 1% = 10_000).
    uint256 public constant BURN_CUT_PPM = 10000;

    /// @notice Fraction retained by the data service as revenue (PPM).
    uint256 public constant DATA_SERVICE_CUT_PPM = 10000;

    /// @notice Absolute lower bound on the thawing period.
    uint64 public constant MIN_THAWING_PERIOD = 14 days;

    /// @notice Stake locked per GRT of fees collected. Matches SubgraphService.
    uint256 public constant STAKE_TO_FEES_RATIO = 5;

    // -------------------------------------------------------------------------
    // Storage
    // -------------------------------------------------------------------------

    /// @notice Whether a provider has registered with this service.
    mapping(address => bool) public registeredProviders;

    /// @notice Address that receives collected GRT for each provider.
    mapping(address => address) public paymentsDestination;

    /// @notice Nest offerings per provider (active and historical).
    mapping(address => NestOffering[]) internal _offerings;

    /// @notice GraphTallyCollector used to redeem TAP receipts on-chain.
    IGraphTallyCollector private immutable GRAPH_TALLY_COLLECTOR;

    /// @notice Governance-adjustable thawing period (lower-bounded by MIN_THAWING_PERIOD).
    uint64 public minThawingPeriod;

    /// @dev Reserved storage slots for future upgrades.
    uint256[50] private __gap;

    // -------------------------------------------------------------------------
    // Constructor
    // -------------------------------------------------------------------------

    /// @dev Sets immutables and locks the implementation against direct initialisation.
    constructor(address controller, address graphTallyCollector) DataService(controller) {
        GRAPH_TALLY_COLLECTOR = IGraphTallyCollector(graphTallyCollector);
        _disableInitializers();
    }

    // -------------------------------------------------------------------------
    // Initializer
    // -------------------------------------------------------------------------

    /// @notice Initialise the proxy. Called exactly once via ERC1967Proxy deployment.
    function initialize(address owner_, address pauseGuardian) external initializer {
        __Ownable_init(owner_);
        __DataService_init();
        __DataServicePausable_init();

        minThawingPeriod = MIN_THAWING_PERIOD;
        _setProvisionTokensRange(MIN_PROVISION, type(uint256).max);
        _setThawingPeriodRange(MIN_THAWING_PERIOD, type(uint64).max);
        _setVerifierCutRange(0, uint32(1_000_000));
        _setPauseGuardian(pauseGuardian, true);
    }

    // -------------------------------------------------------------------------
    // UUPS
    // -------------------------------------------------------------------------

    function _authorizeUpgrade(address) internal override onlyOwner {}

    // -------------------------------------------------------------------------
    // Governance
    // -------------------------------------------------------------------------

    /// @inheritdoc INuthatchDataService
    function setMinThawingPeriod(uint64 period) external onlyOwner {
        if (period < MIN_THAWING_PERIOD) revert ThawingPeriodTooShort(MIN_THAWING_PERIOD, period);
        minThawingPeriod = period;
        emit MinThawingPeriodSet(period);
    }

    /// @inheritdoc INuthatchDataService
    function withdrawFees(address to, uint256 amount) external onlyOwner {
        require(to != address(0), "zero address");
        _graphToken().transfer(to, amount);
        emit FeesWithdrawn(to, amount);
    }

    /// @notice Grant or revoke pause guardian status.
    function setPauseGuardian(address guardian, bool allowed) external onlyOwner {
        _setPauseGuardian(guardian, allowed);
    }

    // -------------------------------------------------------------------------
    // IDataService — provider lifecycle
    // -------------------------------------------------------------------------

    /// @notice Register as a provider.
    /// @param data ABI-encoded (string endpoint, string geoHash, address paymentsDestination).
    function register(address serviceProvider, bytes calldata data)
        external
        override
        whenNotPaused
        onlyAuthorizedForProvision(serviceProvider)
    {
        if (registeredProviders[serviceProvider]) revert ProviderAlreadyRegistered(serviceProvider);

        _checkProvisionTokens(serviceProvider);
        _checkProvisionParameters(serviceProvider, false);

        (string memory endpoint, string memory geoHash, address dest) = abi.decode(data, (string, string, address));

        registeredProviders[serviceProvider] = true;
        paymentsDestination[serviceProvider] = dest == address(0) ? serviceProvider : dest;

        emit ProviderRegistered(serviceProvider, endpoint, geoHash);
    }

    /// @notice Deregister. All active services must be stopped first.
    /// @dev Not in IDataService — no override keyword.
    function deregister(address serviceProvider, bytes calldata) external onlyAuthorizedForProvision(serviceProvider) {
        if (!registeredProviders[serviceProvider]) revert ProviderNotRegistered(serviceProvider);
        if (activeServiceCount(serviceProvider) > 0) revert ActiveServicesExist(serviceProvider);

        registeredProviders[serviceProvider] = false;
        emit ProviderDeregistered(serviceProvider);
    }

    /// @inheritdoc INuthatchDataService
    function setPaymentsDestination(address destination) external {
        if (!registeredProviders[msg.sender]) revert ProviderNotRegistered(msg.sender);
        address dest = destination == address(0) ? msg.sender : destination;
        paymentsDestination[msg.sender] = dest;
        emit PaymentsDestinationSet(msg.sender, dest);
    }

    /// @notice Activate a reproducible Nuthatch nest for one query mode.
    /// @param data ABI-encoded (bytes32 nid, QueryMode mode, string endpoint).
    function startService(address serviceProvider, bytes calldata data)
        external
        override
        whenNotPaused
        onlyAuthorizedForProvision(serviceProvider)
    {
        if (!registeredProviders[serviceProvider]) revert ProviderNotRegistered(serviceProvider);

        (bytes32 nid, QueryMode mode, string memory endpoint) = abi.decode(data, (bytes32, QueryMode, string));
        if (nid == bytes32(0)) revert InvalidNid();

        // Reuse an existing (stopped) slot for this NID/mode pair to keep the array bounded.
        NestOffering[] storage offerings = _offerings[serviceProvider];
        for (uint256 i = 0; i < offerings.length; i++) {
            if (offerings[i].nid == nid && offerings[i].mode == mode) {
                offerings[i].endpoint = endpoint;
                offerings[i].active = true;
                emit OfferingStarted(serviceProvider, nid, mode, endpoint);
                return;
            }
        }

        offerings.push(NestOffering({nid: nid, mode: mode, endpoint: endpoint, active: true}));
        emit OfferingStarted(serviceProvider, nid, mode, endpoint);
    }

    /// @notice Deactivate one NID/query-mode offering.
    /// @param data ABI-encoded (bytes32 nid, QueryMode mode).
    function stopService(address serviceProvider, bytes calldata data)
        external
        override
        onlyAuthorizedForProvision(serviceProvider)
    {
        (bytes32 nid, QueryMode mode) = abi.decode(data, (bytes32, QueryMode));

        NestOffering[] storage offerings = _offerings[serviceProvider];
        for (uint256 i = 0; i < offerings.length; i++) {
            if (offerings[i].nid == nid && offerings[i].mode == mode && offerings[i].active) {
                offerings[i].active = false;
                emit OfferingStopped(serviceProvider, nid, mode);
                return;
            }
        }
        revert OfferingNotFound(serviceProvider, nid, mode);
    }

    /// @notice Collect fees by submitting a signed Receipt Aggregate Voucher (RAV).
    /// @param data ABI-encoded (SignedRAV, tokensToCollect).
    function collect(address serviceProvider, IGraphPayments.PaymentTypes paymentType, bytes calldata data)
        external
        override
        whenNotPaused
        returns (uint256 fees)
    {
        if (paymentType != IGraphPayments.PaymentTypes.QueryFee) revert InvalidPaymentType();
        if (!registeredProviders[serviceProvider]) revert ProviderNotRegistered(serviceProvider);

        (IGraphTallyCollector.SignedRAV memory signedRav, uint256 tokensToCollect) =
            abi.decode(data, (IGraphTallyCollector.SignedRAV, uint256));

        if (signedRav.rav.serviceProvider != serviceProvider) {
            revert InvalidServiceProvider(serviceProvider, signedRav.rav.serviceProvider);
        }

        // Release expired stake claims before locking new ones.
        _releaseStake(serviceProvider, 0);

        uint256 balanceBefore = _graphToken().balanceOf(address(this));
        fees = GRAPH_TALLY_COLLECTOR.collect(
            paymentType,
            abi.encode(signedRav, BURN_CUT_PPM + DATA_SERVICE_CUT_PPM, paymentsDestination[serviceProvider]),
            tokensToCollect
        );

        uint256 received = _graphToken().balanceOf(address(this)) - balanceBefore;
        if (received > 0) {
            uint256 burned = received * BURN_CUT_PPM / (BURN_CUT_PPM + DATA_SERVICE_CUT_PPM);
            _graphToken().burn(burned);
            emit FeesBurned(serviceProvider, burned);
        }

        if (fees > 0) {
            _lockStake(serviceProvider, fees * STAKE_TO_FEES_RATIO, block.timestamp + minThawingPeriod);
        }
    }

    /// @notice Slash is not implemented — this service has no on-chain dispute mechanism.
    function slash(address, bytes calldata) external pure override {
        revert("slashing not supported");
    }

    /// @notice Accept pending provision parameter changes.
    function acceptProvisionPendingParameters(address serviceProvider, bytes calldata)
        external
        override
        onlyAuthorizedForProvision(serviceProvider)
    {
        _acceptProvisionParameters(serviceProvider);
    }

    // -------------------------------------------------------------------------
    // Views
    // -------------------------------------------------------------------------

    /// @inheritdoc INuthatchDataService
    function isRegistered(address provider) external view returns (bool) {
        return registeredProviders[provider];
    }

    /// @inheritdoc INuthatchDataService
    function getServiceRegistrations(address provider) external view returns (NestOffering[] memory) {
        return _offerings[provider];
    }

    /// @inheritdoc INuthatchDataService
    function offeringKey(bytes32 nid, QueryMode mode) external pure returns (bytes32) {
        return keccak256(abi.encode(nid, mode));
    }

    /// @inheritdoc INuthatchDataService
    function activeServiceCount(address provider) public view returns (uint256 count) {
        NestOffering[] storage offerings = _offerings[provider];
        for (uint256 i = 0; i < offerings.length; i++) {
            if (offerings[i].active) count++;
        }
    }
}
