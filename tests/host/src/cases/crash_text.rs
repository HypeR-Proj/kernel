// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

#[path = "../../../../src/kernel/crash/fixed_text.rs"]
mod model;

use core::fmt::Write;

use model::FixedText;

const TEXT_CAPACITY: usize = 256;
const CRASH_REASON_CAPACITY: usize = 512;
type CrashText = FixedText<TEXT_CAPACITY>;

#[test]
fn captures_empty_and_exact_capacity_text() {
    let empty = CrashText::capture(format_args!(""));
    assert_eq!(empty.as_str(), "");
    assert!(!empty.was_truncated());

    let exact = "x".repeat(TEXT_CAPACITY);
    let text = CrashText::capture(format_args!("{exact}"));
    assert_eq!(text.as_str(), exact);
    assert!(!text.was_truncated());
}

#[test]
fn truncates_overlong_text_without_splitting_utf8() {
    let prefix = "x".repeat(TEXT_CAPACITY - 1);
    let value = std::format!("{prefix}é");
    let text = CrashText::capture(format_args!("{value}"));

    assert_eq!(text.as_str(), prefix);
    assert!(text.was_truncated());
    assert!(core::str::from_utf8(text.as_str().as_bytes()).is_ok());
}

#[test]
fn later_format_fragments_cannot_overrun_a_full_buffer() {
    let exact = "x".repeat(TEXT_CAPACITY);
    let text = CrashText::capture(format_args!("{exact}tail"));

    assert_eq!(text.as_str(), exact);
    assert!(text.was_truncated());
}

#[test]
fn utf8_boundary_truncation_closes_the_text_to_later_literals() {
    let prefix = "x".repeat(TEXT_CAPACITY - 1);
    let mut text = CrashText::new();

    assert!(write!(text, "{prefix}é").is_ok());
    assert!(text.was_truncated());
    assert!(text.write_str("later").is_ok());

    assert_eq!(text.as_str(), prefix);
}

#[test]
fn truncated_crash_reason_cannot_splice_in_a_terminal_text() {
    let prefix = "x".repeat(CRASH_REASON_CAPACITY - 1);
    let mut reason = FixedText::<CRASH_REASON_CAPACITY>::new();

    assert!(write!(reason, "{prefix}é").is_ok());
    assert!(reason.was_truncated());
    assert!(reason.write_str("\nlater diagnostic fragment").is_ok());

    assert_eq!(reason.as_str(), prefix);
}
