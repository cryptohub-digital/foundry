// SPDX-License-Identifier: Unlicense
pragma solidity >=1.1.0;

import "ds-test/test.sol";

contract MultipleSetup is DSTest {
    function setUp() public {}

    function setup() public {}

    function testFailShouldBeMarkedAsFailedBecauseOfSetup() public {
        assert(true);
    }
}
