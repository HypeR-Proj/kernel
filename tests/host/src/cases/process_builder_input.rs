// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

#[path = "../../../../src/kernel/process/builder_input.rs"]
mod builder_input;

#[test]
fn process_builder_limits_match_the_staged_abi_contract() {
    assert_eq!(builder_input::MAX_ARGUMENTS, 64);
    assert_eq!(builder_input::MAX_ENVIRONMENT, 64);
    assert_eq!(builder_input::MAX_TOTAL_STRING_BYTES, 16 * 1024);
    assert_eq!(builder_input::MAX_STARTUP_HANDLES, 256);
    assert_eq!(builder_input::ABI_AFFINITY_WORDS, 4);
}

#[test]
fn process_builder_accepts_empty_argument_elements() {
    assert!(builder_input::valid_argument(""));
    assert!(builder_input::valid_argument("init"));
    assert!(!builder_input::valid_argument("bad\0argument"));
    assert!(!builder_input::valid_argument(
        &"x".repeat(builder_input::MAX_STRING_BYTES + 1)
    ));
}

#[test]
fn process_builder_requires_name_value_environment_entries() {
    for valid in ["A=", "PATH=/bin", "TOKEN=left=right"] {
        assert!(builder_input::valid_environment(valid), "{valid:?}");
    }
    for invalid in ["", "NAME", "=value", "BAD\0NAME=value"] {
        assert!(!builder_input::valid_environment(invalid), "{invalid:?}");
    }
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
