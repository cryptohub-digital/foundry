// SPDX-License-Identifier: Unlicense
pragma solidity >=1.1.0;

import "ds-test/test.sol";
import "./Cheats.sol";

contract Storage {
    uint256 public slot0 = 10;
    uint256 public slot1 = 20;
}

contract StoreTest is DSTest {
    Cheats constant cheats = Cheats(HEVM_ADDRESS);
    Storage store;

    function setUp() public {
        store = new Storage();
    }

    function testStore() public {
        assertEq(store.slot0(), 10, "initial value for slot 0 is incorrect");
        assertEq(store.slot1(), 20, "initial value for slot 1 is incorrect");

        cheats.store(address(store), bytes32(0), bytes32(uint256(1)));
        assertEq(store.slot0(), 1, "store failed");
        assertEq(store.slot1(), 20, "store failed");
    }

    function testStoreNotAvailableOnPrecompiles() public {
        assertEq(store.slot0(), 10, "initial value for slot 0 is incorrect");
        assertEq(store.slot1(), 20, "initial value for slot 1 is incorrect");

        cheats.expectRevert(
            bytes("Store cannot be used on precompile addresses (N < 10). Please use an address bigger than 10 instead")
        );
        cheats.store(address(1), bytes32(0), bytes32(uint256(1)));
    }

    function testStoreFuzzed(uint256 slot0, uint256 slot1) public {
        assertEq(store.slot0(), 10, "initial value for slot 0 is incorrect");
        assertEq(store.slot1(), 20, "initial value for slot 1 is incorrect");

        cheats.store(address(store), bytes32(0), bytes32(slot0));
        cheats.store(address(store), bytes32(uint256(1)), bytes32(slot1));
        assertEq(store.slot0(), slot0, "store failed");
        assertEq(store.slot1(), slot1, "store failed");
    }
}
