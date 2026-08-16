// SPDX-License-Identifier: Apache-2.0
pragma solidity ^0.8.27;

import {Test} from "forge-std/Test.sol";
import {ERC1967Proxy} from "@openzeppelin/contracts/proxy/ERC1967/ERC1967Proxy.sol";

import {MockGRTToken} from "@graphprotocol/horizon/mocks/MockGRTToken.sol";
import {ControllerMock} from "@graphprotocol/horizon/mocks/ControllerMock.sol";
import {IHorizonStakingTypes} from "@graphprotocol/interfaces/contracts/horizon/internal/IHorizonStakingTypes.sol";

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
        return
            IHorizonStakingTypes.DelegationPool({
                tokens: 0, shares: 0, tokensThawing: 0, sharesThawing: 0, thawingNonce: 0
            });
    }
    function slash(address, uint256, uint256, address) external {}
    function acceptProvisionParameters(address) external {}
}

contract NuthatchDataServiceTest is Test {
    NuthatchDataService internal service;
    MockStaking internal staking;

    address internal owner = address(this);
    address internal provider = address(0xBEEF);

    function setUp() public {
        MockGRTToken grt = new MockGRTToken();
        ControllerMock controller = new ControllerMock(address(this));
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

        // GraphTallyCollector immutable is only used by collect(); a stub is fine here.
        NuthatchDataService impl = new NuthatchDataService(address(controller), address(1));
        bytes memory initData = abi.encodeCall(NuthatchDataService.initialize, (owner, owner));
        service = NuthatchDataService(address(new ERC1967Proxy(address(impl), initData)));

        // Provider provisions well above MIN_PROVISION, with a valid thawing period.
        staking.setProvision(provider, address(service), 1_000_000e18, 14 days);
    }

    function test_constants() public view {
        assertEq(service.MIN_PROVISION(), 555e18);
        assertEq(service.BURN_CUT_PPM(), 10000);
        assertEq(service.DATA_SERVICE_CUT_PPM(), 10000);
        assertEq(service.STAKE_TO_FEES_RATIO(), 5);
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
}
