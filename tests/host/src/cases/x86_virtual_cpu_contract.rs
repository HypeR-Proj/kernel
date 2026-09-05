// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Architecture-independent x86 vCPU, VMX, CPUID, and MSR contracts.

use hyper::vm::x86::vmx::{
    ControlCapability, EptAccess, EptViolation, IoDirection, IoExit, VmxBasic,
    VmxPhysicalAddressWidth,
};
use hyper::vm::x86::{CpuidResult, GuestMsr};

#[test]
fn classifies_the_supported_guest_msr_surface() {
    for msr in [
        GuestMsr::TimestampCounter,
        GuestMsr::ApicBase,
        GuestMsr::SysenterCs,
        GuestMsr::SysenterEsp,
        GuestMsr::SysenterEip,
        GuestMsr::Pat,
        GuestMsr::Efer,
        GuestMsr::Star,
        GuestMsr::Lstar,
        GuestMsr::Cstar,
        GuestMsr::Sfmask,
        GuestMsr::FsBase,
        GuestMsr::GsBase,
        GuestMsr::KernelGsBase,
        GuestMsr::TscAux,
    ] {
        assert_eq!(GuestMsr::decode(msr.index()), Some(msr));
    }
    assert_eq!(GuestMsr::decode(0xdead_beef), None);
}

#[test]
fn decodes_and_validates_vmx_region_requirements() {
    let raw = 0x1234_u64 | (4096_u64 << 32) | (6 << 50) | (1 << 55);
    let basic = VmxBasic::decode(raw);
    assert_eq!(basic.revision, 0x1234);
    assert_eq!(basic.region_size, 4096);
    assert_eq!(basic.memory_type, 6);
    assert_eq!(
        basic.physical_address_width,
        VmxPhysicalAddressWidth::Processor
    );
    assert!(basic.true_controls);
    assert!(basic.is_supported());
    assert!(basic.accepts_region(0x1234_5000));
    assert!(!basic.accepts_region(0x1234_5001));

    let legacy_width = VmxBasic::decode(raw | (1 << 48));
    assert_eq!(
        legacy_width.physical_address_width,
        VmxPhysicalAddressWidth::Bits32
    );
    assert!(legacy_width.accepts_region(0xffff_f000));
    assert!(!legacy_width.accepts_region(0x1_0000_0000));
}

#[test]
fn applies_fixed_and_optional_control_bits() {
    let capability = ControlCapability::decode((0b1111_u64 << 32) | 0b0001);
    assert_eq!(capability.apply(0b0110), Some(0b0111));

    let restricted = ControlCapability::decode((0b0011_u64 << 32) | 0b0001);
    assert_eq!(restricted.apply(0b0100), None);
}

#[test]
fn decodes_io_and_ept_exit_qualifications() {
    let io = crate::require_some(IoExit::decode((u64::from(0x3f8_u16) << 16) | (1 << 3)));
    assert_eq!(io.port, 0x3f8);
    assert_eq!(io.size, 1);
    assert_eq!(io.direction, IoDirection::Input);
    assert!(!io.string);
    assert!(IoExit::decode(2).is_none());

    let ept = EptViolation::decode((1 << 2) | (1 << 8));
    assert_eq!(ept.access, EptAccess::Execute);
    assert!(!ept.during_page_walk);
    let page_walk = EptViolation::decode(1 << 1);
    assert_eq!(page_walk.access, EptAccess::Write);
    assert!(page_walk.during_page_walk);
}

#[test]
fn hides_unmanaged_guest_state_from_cpuid() {
    let input = CpuidResult {
        eax: u32::MAX,
        ebx: u32::MAX,
        ecx: u32::MAX,
        edx: u32::MAX,
    };
    let leaf1 = hyper::vm::x86::sanitize_cpuid(1, 0, input);
    assert_eq!((leaf1.ebx >> 16) & 0xff, 1);
    assert_eq!(leaf1.ecx & (1 << 5), 0);
    assert_eq!(leaf1.ecx & (1 << 28), 0);
    assert_ne!(leaf1.ecx & (1 << 31), 0);
    assert_eq!(leaf1.edx & (1 << 9), 0);

    let topology = hyper::vm::x86::sanitize_cpuid(0x0b, 0, input);
    assert_eq!(topology, CpuidResult::ZERO);
    let xsave = hyper::vm::x86::sanitize_cpuid(0x0d, 0, input);
    assert_eq!(xsave, CpuidResult::ZERO);
    let structured = hyper::vm::x86::sanitize_cpuid(7, 0, input);
    assert_eq!(structured.ebx & (1 << 2), 0);
    assert_eq!(structured.ebx & (1 << 25), 0);
    assert_eq!(structured.eax, 0);
    assert_eq!(
        hyper::vm::x86::sanitize_cpuid(7, 1, input),
        CpuidResult::ZERO
    );
    let extended_features = hyper::vm::x86::sanitize_cpuid(0x8000_0008, 0, input);
    assert_eq!(extended_features.eax, u32::MAX);
    assert_eq!(extended_features.ebx, 0);
    assert_eq!(extended_features.ecx, 0);
    assert_eq!(extended_features.edx, 0);
    let hypervisor = hyper::vm::x86::hypervisor_cpuid();
    assert_eq!(hypervisor.eax, 0x4000_0000);
    let mut vendor = [0_u8; 12];
    vendor[0..4].copy_from_slice(&hypervisor.ebx.to_le_bytes());
    vendor[4..8].copy_from_slice(&hypervisor.ecx.to_le_bytes());
    vendor[8..12].copy_from_slice(&hypervisor.edx.to_le_bytes());
    assert_eq!(&vendor, b"HypeR HyperV");

    let basic = hyper::vm::x86::sanitize_cpuid(0, 0, input);
    let mut cpu_vendor = [0_u8; 12];
    cpu_vendor[0..4].copy_from_slice(&basic.ebx.to_le_bytes());
    cpu_vendor[4..8].copy_from_slice(&basic.edx.to_le_bytes());
    cpu_vendor[8..12].copy_from_slice(&basic.ecx.to_le_bytes());
    assert_eq!(&cpu_vendor, b"HypeR CPU   ");
    let extended = hyper::vm::x86::sanitize_cpuid(0x8000_0000, 0, input);
    assert_eq!(extended.eax, 0x8000_0008);
}

#[test]
fn applies_x86_port_input_accumulator_semantics() {
    let initial = 0x1122_3344_5566_7788;
    assert_eq!(
        hyper::vm::x86::merge_port_input(initial, 0xaabb_ccdd, 1),
        Some(0x1122_3344_5566_77dd)
    );
    assert_eq!(
        hyper::vm::x86::merge_port_input(initial, 0xaabb_ccdd, 2),
        Some(0x1122_3344_5566_ccdd)
    );
    assert_eq!(
        hyper::vm::x86::merge_port_input(initial, 0xaabb_ccdd, 4),
        Some(0x0000_0000_aabb_ccdd)
    );
    assert_eq!(hyper::vm::x86::merge_port_input(initial, 0, 8), None);
}
