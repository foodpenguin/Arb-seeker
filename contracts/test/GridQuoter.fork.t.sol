// SPDX-License-Identifier: AGPL-3.0-or-later
pragma solidity 0.8.28;

import {Test} from "forge-std/Test.sol";
import {GridQuoter} from "../src/sim/GridQuoter.sol";

/// GridQuoter's per-hop gas cap on a Base fork: the new-AERO/USDC tick-spacing-1 Slipstream pool
/// (~$252 USDC deep on 10/27) took 17.6M gas per drained quote, so the aero sell ladder (16 sizes)
/// went past ankr's 100M eth_call gas cap and the whole quote multicall reverted.
contract GridQuoterForkTest is Test {
    address constant NEW_AERO_USDC_CL1 = 0xb27460d0e2B4eC0fd2b2c29f0c2B29D4C045f863; // new AERO = token0
    GridQuoter quoter;

    function setUp() public {
        string memory rpc = vm.envOr("BASE_RPC_URL", string("https://base.gateway.tenderly.co"));
        vm.createSelectFork(rpc, vm.envOr("FORK_BLOCK_QUOTER", uint256(53_159_400)));
        quoter = new GridQuoter();
    }

    function test_drainingQuotesStopAtTheHopGasCap() public {
        GridQuoter.Path[] memory paths = new GridQuoter.Path[](1);
        address[] memory pools = new address[](1);
        pools[0] = NEW_AERO_USDC_CL1;
        bool[] memory zfo = new bool[](1);
        zfo[0] = true;
        paths[0] = GridQuoter.Path({pools: pools, zeroForOne: zfo});
        uint256[] memory amts = new uint256[](16); // the aero sell ladder: 10 x 2^k AERO
        for (uint256 k; k < 16; ++k) {
            amts[k] = (10 << k) * 1e18;
        }

        uint256 g = gasleft();
        (uint256[][] memory outs, uint256[][] memory gasUsed) = quoter.quote(paths, amts);
        uint256 total = g - gasleft();

        assertLt(total, 10_000_000, "whole ladder well under every provider's eth_call gas cap");
        assertGt(outs[0][0], 0, "small sizes still quote");
        for (uint256 k = 1; k < 16; ++k) {
            if (outs[0][k] == 0) {
                for (uint256 j = k; j < 16; ++j) {
                    assertEq(outs[0][j], 0, "past the cap every larger size is unquoted");
                }
                break;
            }
            assertGt(outs[0][k], outs[0][k - 1], "quotes grow with size below the cap");
            assertLt(gasUsed[0][k], 3_100_000, "a quoted size fits the hop cap");
        }
        assertEq(outs[0][15], 0, "the draining sizes are dropped");
    }
}
