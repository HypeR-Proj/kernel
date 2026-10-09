// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

#[path = "../../../../src/kernel/process/builder_input.rs"]
mod builder_input;

#[test]
fn process_builder_limits_match_the_staged_abi_contract() {
    assert_eq!(builder_input::MAX_STARTUP_DATA_BYTES, 16 * 1024);
    assert_eq!(builder_input::MAX_STARTUP_HANDLES, 256);
    assert_eq!(builder_input::ABI_AFFINITY_WORDS, 4);
}

#[test]
fn process_builder_names_are_nonempty_and_bounded() {
    assert!(builder_input::valid_name("init"));
    assert!(!builder_input::valid_name(""));
    assert!(!builder_input::valid_name("bad\0name"));
    assert!(!builder_input::valid_name(
        &"n".repeat(builder_input::MAX_NAME_BYTES + 1)
    ));
}
