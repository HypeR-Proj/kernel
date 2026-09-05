// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Architecture-independent decoding of AMD SVM capabilities and exits.

use hyper::vm::x86::svm::{IoDirection, IoExit, NptAccess, NptViolation, SvmFeatures};

#[test]
fn validates_required_svm_facilities() {
    let features = SvmFeatures::decode(1, 32, 1 | (1 << 3) | (1 << 5) | (1 << 6));
    assert_eq!(features.revision, 1);
    assert_eq!(features.asids, 32);
    assert!(features.nested_paging);
    assert!(features.next_rip);
    assert!(features.vmcb_clean);
    assert!(features.flush_by_asid);
    assert!(features.supports_backend());

    assert!(!SvmFeatures::decode(1, 32, 1).supports_backend());
    assert!(!SvmFeatures::decode(1, 1, 1 | (1 << 3)).supports_backend());
}

#[test]
fn decodes_io_and_nested_page_fault_exits() {
    let info = (u64::from(0x3f8_u16) << 16) | (1 << 4) | 1;
    let io = crate::require_some(IoExit::decode(info));
    assert_eq!(io.port, 0x3f8);
    assert_eq!(io.size, 1);
    assert_eq!(io.direction, IoDirection::Input);
    assert!(!io.string);
    assert!(IoExit::decode(3 << 4).is_none());

    let execute = NptViolation::decode((1 << 4) | (1 << 33));
    assert_eq!(execute.access, NptAccess::Execute);
    assert!(execute.during_page_walk);
    let write = NptViolation::decode(1 << 1);
    assert_eq!(write.access, NptAccess::Write);
    assert!(!write.during_page_walk);
}
