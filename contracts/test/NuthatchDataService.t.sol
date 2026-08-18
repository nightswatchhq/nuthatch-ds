// SPDX-License-Identifier: Apache-2.0
pragma solidity ^0.8.27;

import {Test} from "forge-std/Test.sol";
import {ERC1967Proxy} from "@openzeppelin/contracts/proxy/ERC1967/ERC1967Proxy.sol";

import {MockGRTToken} from "@graphprotocol/horizon/mocks/MockGRTToken.sol";
import {ControllerMock} from "@graphprotocol/horizon/mocks/ControllerMock.sol";
import {IHorizonStakingTypes} from "@graphprotocol/interfaces/contracts/horizon/internal/IHorizonStakingTypes.sol";
import {IGraphPayments} from "@graphprotocol/interfaces/contracts/horizon/IGraphPayments.sol";
import {IGraphTallyCollector} from "@graphprotocol/interfaces/contracts/horizon/IGraphTallyCollector.sol";
import {ProvisionTracker} from "@graphprotocol/horizon/data-service/libraries/ProvisionTracker.sol";

import {NuthatchDataService} from "../src/NuthatchDataService.sol";
import {INuthatchDataService} from "../src/interfaces/INuthatchDataService.sol";

/// @dev Minimal HorizonStaking mock — just enough for register/start/stop lifecycle.
contract MockStaking {
    mapping(address => mapping(address => IHorizonStakingTypes.Provision)) public provisions;

    function setProvision(address sp, address ds, uint256 tokens, uint64 thawingPeriod) external {
        provisions[sp][ds] = IHorizonStakingTypes.Provision({
            tokens: tokens,
            tokensThawing: 0,
            sharesThawing: 0,
            maxVerifierCut: 1_000_000,
            thawingPeriod: thawingPeriod,
            createdAt: uint64(block.timestamp),
            maxVerifierCutPending: 0,
            thawingPeriodPending: 0,
            lastParametersStagedAt: 0,
            thawingNonce: 0
        });
    }

    function getProvision(address sp, address ds) external view returns (IHorizonStakingTypes.Provision memory) {
        return provisions[sp][ds];
    }

    function isAuthorized(address sp, address, address op) external pure returns (bool) {
        return sp == op;
    }

    function getTokensAvailable(address sp, address ds, uint32) external view returns (uint256) {
        return provisions[sp][ds].tokens;
    }

    function getDelegationPool(address, address) external pure returns (IHorizonStakingTypes.DelegationPool memory) {
        return IHorizonStakingTypes.DelegationPool({
            tokens: 0, shares: 0, tokensThawing: 0, sharesThawing: 0, thawingNonce: 0
        });
    }
    function slash(address, uint256, uint256, address) external {}
    function acceptProvisionParameters(address) external {}
}

/// @dev Stands in for GraphTallyCollector + GraphPayments + PaymentsEscrow.
///      Mints the caller its PPM cut of the collected amount, exactly as the real
///      escrow route does, and returns the gross figure the real collector returns.
contract MockTallyCollector {
    MockGRTToken public immutable GRT;

    constructor(MockGRTToken grt) {
        GRT = grt;
    }

    function collect(IGraphPayments.PaymentTypes, bytes calldata data, uint256 tokensToCollect)
        external
        returns (uint256)
    {
        (, uint256 dataServiceCut,) = abi.decode(data, (IGraphTallyCollector.SignedRAV, uint256, address));
        GRT.mint(msg.sender, tokensToCollect * dataServiceCut / 1_000_000);
        return tokensToCollect;
    }
}

contract NuthatchDataServiceTest is Test {
    NuthatchDataService internal service;
    MockStaking internal staking;
    ControllerMock internal controller;
    MockGRTToken internal grt;
    MockTallyCollector internal tally;

    address internal owner = address(this);
    address internal provider = address(0xBEEF);

    function setUp() public {
        grt = new MockGRTToken();
        tally = new MockTallyCollector(grt);
        controller = new ControllerMock(address(this));
        staking = new MockStaking();

        controller.setContractProxy(keccak256("GraphToken"), address(grt));
        controller.setContractProxy(keccak256("Staking"), address(staking));
        controller.setContractProxy(keccak256("EpochManager"), address(1));
        controller.setContractProxy(keccak256("RewardsManager"), address(1));
        controller.setContractProxy(keccak256("GraphTokenGateway"), address(1));
        controller.setContractProxy(keccak256("GraphProxyAdmin"), address(1));
        controller.setContractProxy(keccak256("Curation"), address(1));
        controller.setContractProxy(keccak256("GraphPayments"), address(1));
        controller.setContractProxy(keccak256("PaymentsEscrow"), address(1));

        NuthatchDataService impl = new NuthatchDataService(address(controller), address(tally));
        bytes memory initData = abi.encodeCall(NuthatchDataService.initialize, (owner, owner));
        service = NuthatchDataService(address(new ERC1967Proxy(address(impl), initData)));

        // Provider uses a valid thawing period. The soft-launch provision floor is zero.
        staking.setProvision(provider, address(service), 1_000_000e18, 14 days);
    }

    function test_constants() public view {
        assertEq(service.MIN_PROVISION(), 0);
        assertEq(service.BURN_CUT_PPM(), 10000);
        assertEq(service.DATA_SERVICE_CUT_PPM(), 10000);
        assertEq(service.STAKE_TO_FEES_RATIO(), 5);
    }

    function test_ownerCanSetMinimumProvisionTokens() public {
        service.setMinimumProvisionTokens(123e18);
        (uint256 minimum, uint256 maximum) = service.getProvisionTokensRange();
        assertEq(minimum, 123e18);
        assertEq(maximum, type(uint256).max);
    }

    function test_upgradeCanMigrateProvisionFloorWithoutLosingOfferings() public {
        vm.startPrank(provider);
        service.register(provider, abi.encode("https://p", "geo", address(0)));
        bytes32 nid = keccak256("horizon-nest");
        service.startService(provider, abi.encode(nid, INuthatchDataService.QueryMode.NAMED, "https://p"));
        vm.stopPrank();

        service.setMinimumProvisionTokens(555e18);
        NuthatchDataService implementationV2 = new NuthatchDataService(address(controller), address(tally));
        service.upgradeToAndCall(
            address(implementationV2), abi.encodeCall(NuthatchDataService.setMinimumProvisionTokens, (0))
        );

        (uint256 minimum,) = service.getProvisionTokensRange();
        assertEq(minimum, 0);
        assertTrue(service.isRegistered(provider));
        assertEq(service.activeServiceCount(provider), 1);
        INuthatchDataService.NestOffering[] memory offerings = service.getServiceRegistrations(provider);
        assertEq(offerings[0].nid, nid);
    }

    function test_register() public {
        vm.prank(provider);
        service.register(provider, abi.encode("https://provider.example", "u4pruyd", address(0)));

        assertTrue(service.isRegistered(provider));
        // paymentsDestination defaults to the provider when address(0) passed.
        assertEq(service.paymentsDestination(provider), provider);
    }

    function test_register_revertsOnDuplicate() public {
        vm.startPrank(provider);
        service.register(provider, abi.encode("https://p", "geo", address(0)));
        vm.expectRevert(abi.encodeWithSelector(INuthatchDataService.ProviderAlreadyRegistered.selector, provider));
        service.register(provider, abi.encode("https://p", "geo", address(0)));
        vm.stopPrank();
    }

    function test_register_allowsZeroProvision() public {
        address zeroProvisionProvider = address(0xCAFE);
        staking.setProvision(zeroProvisionProvider, address(service), 0, 14 days);

        vm.prank(zeroProvisionProvider);
        service.register(zeroProvisionProvider, abi.encode("https://p", "geo", address(0)));

        assertTrue(service.isRegistered(zeroProvisionProvider));
    }

    function test_startAndStopOffering() public {
        vm.startPrank(provider);
        service.register(provider, abi.encode("https://p", "geo", address(0)));
        bytes32 nid = keccak256("horizon-nest");

        service.startService(provider, abi.encode(nid, INuthatchDataService.QueryMode.NAMED, "https://p/named"));
        assertEq(service.activeServiceCount(provider), 1);

        INuthatchDataService.NestOffering[] memory regs = service.getServiceRegistrations(provider);
        assertEq(regs.length, 1);
        assertEq(regs[0].nid, nid);
        assertTrue(regs[0].active);

        service.stopService(provider, abi.encode(nid, INuthatchDataService.QueryMode.NAMED));
        assertEq(service.activeServiceCount(provider), 0);
        vm.stopPrank();
    }

    function test_providerCanOfferBothModesForOneNest() public {
        vm.startPrank(provider);
        service.register(provider, abi.encode("https://p", "geo", address(0)));
        bytes32 nid = keccak256("horizon-nest");
        service.startService(provider, abi.encode(nid, INuthatchDataService.QueryMode.NAMED, "https://p"));
        service.startService(provider, abi.encode(nid, INuthatchDataService.QueryMode.SQL, "https://p"));
        assertEq(service.activeServiceCount(provider), 2);
        assertTrue(
            service.offeringKey(nid, INuthatchDataService.QueryMode.NAMED)
                != service.offeringKey(nid, INuthatchDataService.QueryMode.SQL)
        );
        vm.stopPrank();
    }

    function test_restartingOfferingUpdatesRatherThanDuplicates() public {
        vm.startPrank(provider);
        service.register(provider, abi.encode("https://p", "geo", address(0)));
        bytes32 nid = keccak256("horizon-nest");
        service.startService(provider, abi.encode(nid, INuthatchDataService.QueryMode.NAMED, "https://one"));
        service.stopService(provider, abi.encode(nid, INuthatchDataService.QueryMode.NAMED));
        service.startService(provider, abi.encode(nid, INuthatchDataService.QueryMode.NAMED, "https://two"));

        INuthatchDataService.NestOffering[] memory offerings = service.getServiceRegistrations(provider);
        assertEq(offerings.length, 1);
        assertEq(offerings[0].endpoint, "https://two");
        assertTrue(offerings[0].active);
        assertEq(service.activeServiceCount(provider), 1);
        vm.stopPrank();
    }

    function test_startService_rejectsZeroNid() public {
        vm.startPrank(provider);
        service.register(provider, abi.encode("https://p", "geo", address(0)));
        vm.expectRevert(INuthatchDataService.InvalidNid.selector);
        service.startService(provider, abi.encode(bytes32(0), INuthatchDataService.QueryMode.NAMED, "https://p"));
        vm.stopPrank();
    }

    function test_startService_rejectsUnregisteredProvider() public {
        vm.prank(provider);
        vm.expectRevert(abi.encodeWithSelector(INuthatchDataService.ProviderNotRegistered.selector, provider));
        service.startService(
            provider, abi.encode(keccak256("horizon-nest"), INuthatchDataService.QueryMode.NAMED, "https://p")
        );
    }

    function test_stopService_rejectsWrongOffering() public {
        vm.startPrank(provider);
        service.register(provider, abi.encode("https://p", "geo", address(0)));
        bytes32 nid = keccak256("horizon-nest");
        service.startService(provider, abi.encode(nid, INuthatchDataService.QueryMode.NAMED, "https://p"));
        vm.expectRevert(
            abi.encodeWithSelector(
                INuthatchDataService.OfferingNotFound.selector, provider, nid, INuthatchDataService.QueryMode.SQL
            )
        );
        service.stopService(provider, abi.encode(nid, INuthatchDataService.QueryMode.SQL));
        vm.stopPrank();
    }

    function test_deregister_revertsWithActiveServices() public {
        vm.startPrank(provider);
        service.register(provider, abi.encode("https://p", "geo", address(0)));
        service.startService(
            provider, abi.encode(keccak256("horizon-nest"), INuthatchDataService.QueryMode.NAMED, "https://p")
        );
        vm.expectRevert(abi.encodeWithSelector(INuthatchDataService.ActiveServicesExist.selector, provider));
        service.deregister(provider, "");
        vm.stopPrank();
    }

    function test_providerCanDeregisterAfterStoppingOffering() public {
        vm.startPrank(provider);
        service.register(provider, abi.encode("https://p", "geo", address(0)));
        bytes32 nid = keccak256("horizon-nest");
        service.startService(provider, abi.encode(nid, INuthatchDataService.QueryMode.NAMED, "https://p"));
        service.stopService(provider, abi.encode(nid, INuthatchDataService.QueryMode.NAMED));
        service.deregister(provider, "");
        assertFalse(service.isRegistered(provider));
        vm.stopPrank();
    }

    function test_setPaymentsDestinationDefaultsToProvider() public {
        vm.startPrank(provider);
        service.register(provider, abi.encode("https://p", "geo", address(0xABCD)));
        service.setPaymentsDestination(address(0));
        assertEq(service.paymentsDestination(provider), provider);
        vm.stopPrank();
    }

    function test_slash_reverts() public {
        vm.expectRevert(bytes("slashing not supported"));
        service.slash(provider, "");
    }

    function test_setMinThawingPeriod_belowFloorReverts() public {
        vm.expectRevert(
            abi.encodeWithSelector(INuthatchDataService.ThawingPeriodTooShort.selector, uint64(14 days), uint64(1 days))
        );
        service.setMinThawingPeriod(1 days);
    }

    // -------------------------------------------------------------------------
    // Governance
    // -------------------------------------------------------------------------

    function test_setMinThawingPeriod_movesProvisionAcceptanceRangeToo() public {
        service.setMinThawingPeriod(30 days);
        assertEq(service.minThawingPeriod(), 30 days);

        // The two stores must not drift: a provision thawing for the old floor is
        // no longer acceptable once governance has raised the requirement.
        (uint64 minimum,) = service.getThawingPeriodRange();
        assertEq(minimum, 30 days);

        address thinThaw = address(0xD00D);
        staking.setProvision(thinThaw, address(service), 1e18, 14 days);
        vm.prank(thinThaw);
        vm.expectRevert();
        service.register(thinThaw, abi.encode("https://p", "geo", address(0)));
    }

    function test_withdrawFees() public {
        grt.mint(address(service), 100e18);
        service.withdrawFees(address(0xFEE5), 40e18);
        assertEq(grt.balanceOf(address(0xFEE5)), 40e18);
        assertEq(grt.balanceOf(address(service)), 60e18);
    }

    function test_withdrawFees_rejectsZeroAddress() public {
        grt.mint(address(service), 1e18);
        vm.expectRevert(bytes("zero address"));
        service.withdrawFees(address(0), 1e18);
    }

    function test_withdrawFees_onlyOwner() public {
        grt.mint(address(service), 1e18);
        vm.prank(provider);
        vm.expectRevert();
        service.withdrawFees(provider, 1e18);
    }

    // -------------------------------------------------------------------------
    // Offering bounds
    // -------------------------------------------------------------------------

    function test_startService_enforcesOfferingCap() public {
        vm.startPrank(provider);
        service.register(provider, abi.encode("https://p", "geo", address(0)));
        uint256 cap = service.MAX_OFFERINGS_PER_PROVIDER();
        for (uint256 i = 0; i < cap; i++) {
            service.startService(
                provider, abi.encode(keccak256(abi.encode(i)), INuthatchDataService.QueryMode.NAMED, "https://p")
            );
        }
        vm.expectRevert(abi.encodeWithSelector(INuthatchDataService.TooManyOfferings.selector, provider, cap));
        service.startService(
            provider, abi.encode(keccak256("one too many"), INuthatchDataService.QueryMode.NAMED, "https://p")
        );
        vm.stopPrank();
    }

    // -------------------------------------------------------------------------
    // collect()
    // -------------------------------------------------------------------------

    function _signedRav(address serviceProvider_, uint128 value)
        internal
        view
        returns (IGraphTallyCollector.SignedRAV memory)
    {
        return IGraphTallyCollector.SignedRAV({
            rav: IGraphTallyCollector.ReceiptAggregateVoucher({
                collectionId: keccak256("collection"),
                payer: address(0xBADD1E),
                serviceProvider: serviceProvider_,
                dataService: address(service),
                timestampNs: uint64(block.timestamp) * 1e9,
                valueAggregate: value,
                metadata: ""
            }),
            signature: hex"00"
        });
    }

    function _registerProvider() internal {
        vm.prank(provider);
        service.register(provider, abi.encode("https://p", "geo", address(0)));
    }

    function test_collect_burnsHalfTheCutAndRetainsTheRest() public {
        _registerProvider();
        uint256 supplyBefore = grt.totalSupply();

        // 2% total cut on 1000 GRT = 20 GRT to the service; half of that is burned.
        uint256 fees = service.collect(
            provider, IGraphPayments.PaymentTypes.QueryFee, abi.encode(_signedRav(provider, 1000e18), uint256(1000e18))
        );

        assertEq(fees, 1000e18, "collect returns the gross collected amount");
        assertEq(grt.balanceOf(address(service)), 10e18, "1% retained as data service revenue");
        assertEq(grt.totalSupply(), supplyBefore + 10e18, "1% burned, 1% retained");
    }

    function test_collect_emitsFeesBurned() public {
        _registerProvider();
        vm.expectEmit(true, false, false, true, address(service));
        emit INuthatchDataService.FeesBurned(provider, 10e18);
        service.collect(
            provider, IGraphPayments.PaymentTypes.QueryFee, abi.encode(_signedRav(provider, 1000e18), uint256(1000e18))
        );
    }

    function test_collect_locksStakeAtTheDeclaredRatio() public {
        _registerProvider();
        service.collect(
            provider, IGraphPayments.PaymentTypes.QueryFee, abi.encode(_signedRav(provider, 1000e18), uint256(1000e18))
        );
        assertEq(service.feesProvisionTracker(provider), 1000e18 * 5);
    }

    function test_collect_releasesExpiredClaimsBeforeLockingNewOnes() public {
        _registerProvider();
        service.collect(
            provider, IGraphPayments.PaymentTypes.QueryFee, abi.encode(_signedRav(provider, 1000e18), uint256(1000e18))
        );
        assertEq(service.feesProvisionTracker(provider), 5000e18);

        vm.warp(block.timestamp + 14 days + 1);
        service.collect(
            provider, IGraphPayments.PaymentTypes.QueryFee, abi.encode(_signedRav(provider, 2000e18), uint256(1000e18))
        );
        // The first claim expired and was released; only the second is still locked.
        assertEq(service.feesProvisionTracker(provider), 5000e18);
    }

    function test_collect_revertsWhenProvisionCannotBackTheFees() public {
        // 0.0001 GRT of provision backs 0.00002 GRT of fees at a 5:1 ratio.
        staking.setProvision(provider, address(service), 1e14, 14 days);
        _registerProvider();
        assertEq(service.maxCollectableFees(provider), 2e13);

        vm.expectRevert(
            abi.encodeWithSelector(ProvisionTracker.ProvisionTrackerInsufficientTokens.selector, 1e14, 5e14)
        );
        service.collect(
            provider, IGraphPayments.PaymentTypes.QueryFee, abi.encode(_signedRav(provider, 1e14), uint256(1e14))
        );
    }

    function test_maxCollectableFees_shrinksAsStakeIsLocked() public {
        _registerProvider();
        assertEq(service.maxCollectableFees(provider), 1_000_000e18 / 5);
        service.collect(
            provider, IGraphPayments.PaymentTypes.QueryFee, abi.encode(_signedRav(provider, 1000e18), uint256(1000e18))
        );
        assertEq(service.maxCollectableFees(provider), (1_000_000e18 - 5000e18) / 5);
    }

    function test_collect_rejectsNonQueryFeePaymentTypes() public {
        _registerProvider();
        vm.expectRevert(INuthatchDataService.InvalidPaymentType.selector);
        service.collect(
            provider,
            IGraphPayments.PaymentTypes.IndexingFee,
            abi.encode(_signedRav(provider, 1000e18), uint256(1000e18))
        );
    }

    function test_collect_rejectsUnregisteredProvider() public {
        vm.expectRevert(abi.encodeWithSelector(INuthatchDataService.ProviderNotRegistered.selector, provider));
        service.collect(
            provider, IGraphPayments.PaymentTypes.QueryFee, abi.encode(_signedRav(provider, 1000e18), uint256(1000e18))
        );
    }

    function test_collect_rejectsRavForAnotherServiceProvider() public {
        _registerProvider();
        address other = address(0xB0B);
        vm.expectRevert(abi.encodeWithSelector(INuthatchDataService.InvalidServiceProvider.selector, provider, other));
        service.collect(
            provider, IGraphPayments.PaymentTypes.QueryFee, abi.encode(_signedRav(other, 1000e18), uint256(1000e18))
        );
    }

    function test_collect_rejectedWhilePaused() public {
        _registerProvider();
        service.pause();
        vm.expectRevert();
        service.collect(
            provider, IGraphPayments.PaymentTypes.QueryFee, abi.encode(_signedRav(provider, 1000e18), uint256(1000e18))
        );
    }

    function test_providerCanStillExitWhilePaused() public {
        vm.startPrank(provider);
        service.register(provider, abi.encode("https://p", "geo", address(0)));
        bytes32 nid = keccak256("horizon-nest");
        service.startService(provider, abi.encode(nid, INuthatchDataService.QueryMode.NAMED, "https://p"));
        vm.stopPrank();

        service.pause();

        vm.startPrank(provider);
        service.stopService(provider, abi.encode(nid, INuthatchDataService.QueryMode.NAMED));
        service.deregister(provider, "");
        vm.stopPrank();
        assertFalse(service.isRegistered(provider));
    }
}
