// SPDX-License-Identifier: Apache-2.0
pragma solidity ^0.8.27;

import {Script, console2} from "forge-std/Script.sol";
import {ERC1967Proxy} from "@openzeppelin/contracts/proxy/ERC1967/ERC1967Proxy.sol";
import {NuthatchDataService} from "../src/NuthatchDataService.sol";

/// @notice Deploy NuthatchDataService (UUPS upgradeable proxy).
///
/// Usage — Arbitrum Sepolia (testnet):
///   forge script contracts/script/Deploy.s.sol \
///     --rpc-url arbitrum_sepolia --private-key $PRIVATE_KEY --broadcast --verify -vvvv
///
/// Required env (see .env.example): PRIVATE_KEY, OWNER, PAUSE_GUARDIAN.
/// Optional overrides: GRAPH_CONTROLLER, GRAPH_TALLY_COLLECTOR.
///
/// Horizon addresses — Arbitrum Sepolia (421614):
///   Controller:          0x9DB3ee191681f092607035d9BDA6e59FbEaCa695
///   GraphTallyCollector: 0xacC71844EF6beEF70106ABe6E51013189A1f3738
///   PaymentsEscrow:      0x09B985a2042848A08bA59060EaF0f07c6F5D4d54
///
/// Horizon addresses — Arbitrum One (42161, mainnet):
///   Controller:          cast call 0xb2Bb92d0DE618878E438b55D5846cfecD9301105 "controller()(address)"
///   GraphTallyCollector: 0x8f69F5C07477Ac46FBc491B1E6D91E2bb0111A9e
///   PaymentsEscrow:      0xf6Fcc27aAf1fcD8B254498c9794451d82afC673E
contract Deploy is Script {
    function run() external {
        address owner_        = vm.envAddress("OWNER");
        address pauseGuardian = vm.envAddress("PAUSE_GUARDIAN");

        address controller = vm.envOr("GRAPH_CONTROLLER", address(0x9DB3ee191681f092607035d9BDA6e59FbEaCa695));
        address graphTallyCollector =
            vm.envOr("GRAPH_TALLY_COLLECTOR", address(0xacC71844EF6beEF70106ABe6E51013189A1f3738));

        vm.startBroadcast();

        NuthatchDataService impl = new NuthatchDataService(controller, graphTallyCollector);
        console2.log("NuthatchDataService implementation:", address(impl));

        bytes memory initData =
            abi.encodeCall(NuthatchDataService.initialize, (owner_, pauseGuardian));
        ERC1967Proxy proxy = new ERC1967Proxy(address(impl), initData);
        console2.log("NuthatchDataService proxy deployed at:", address(proxy));

        vm.stopBroadcast();

        console2.log("\nSet in your gateway.toml [tap].data_service_address:");
        console2.log(vm.toString(address(proxy)));
    }
}
