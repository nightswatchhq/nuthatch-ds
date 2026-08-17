// SPDX-License-Identifier: Apache-2.0
pragma solidity ^0.8.27;

import {Script, console2} from "forge-std/Script.sol";
import {NuthatchDataService} from "../src/NuthatchDataService.sol";

/// @notice Upgrade an existing Nuthatch Data Service proxy and migrate its provision floor.
/// @dev Required env: DATA_SERVICE_PROXY, GRAPH_CONTROLLER, GRAPH_TALLY_COLLECTOR.
///      The broadcaster must be the proxy owner.
contract Upgrade is Script {
    function run() external {
        address proxy = vm.envAddress("DATA_SERVICE_PROXY");
        address controller = vm.envAddress("GRAPH_CONTROLLER");
        address graphTallyCollector = vm.envAddress("GRAPH_TALLY_COLLECTOR");
        uint256 minimumProvision = vm.envOr("MINIMUM_PROVISION", uint256(0));

        vm.startBroadcast();
        NuthatchDataService implementation = new NuthatchDataService(controller, graphTallyCollector);
        NuthatchDataService(proxy)
            .upgradeToAndCall(
                address(implementation),
                abi.encodeCall(NuthatchDataService.setMinimumProvisionTokens, (minimumProvision))
            );
        vm.stopBroadcast();

        console2.log("NuthatchDataService implementation:", address(implementation));
        console2.log("NuthatchDataService proxy:", proxy);
        console2.log("Minimum provision:", minimumProvision);
    }
}
