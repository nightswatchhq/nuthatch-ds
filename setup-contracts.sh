#!/usr/bin/env bash
# Vendor the Horizon + OpenZeppelin contract libraries used by NuthatchDataService.
# Run once after generation, from the repo root. Requires Foundry (`foundryup`).
set -euo pipefail

[ -d .git ] || git init -q

# forge install adds these under lib/. The remappings in foundry.toml expect exactly
# these directory names. (Older Foundry needs `--no-commit`; current makes it the default.)
#
# graphprotocol/contracts is PINNED to the @graphprotocol/horizon@1.1.0 tag commit — later
# releases (v6.0.0+) reorganise away from the packages/horizon/contracts/ layout these
# remappings expect. OZ upgradeable v5.6.1 bundles plain OZ under its own lib/.
forge install foundry-rs/forge-std
forge install graphprotocol/contracts@32d09fd45c8d39ac541eadd13dee580e398b9a79
forge install OpenZeppelin/openzeppelin-contracts-upgradeable@v5.6.1

echo
echo "Libraries vendored under lib/. Now:"
echo "  forge build && forge test"
echo
echo "If imports fail to resolve, check the OZ remapping root in foundry.toml against"
echo "the actual lib/ layout (see the gotchas in the create-data-service skill)."
