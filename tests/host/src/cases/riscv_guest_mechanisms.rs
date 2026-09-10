// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

use hyper::vm::exit::{AccessWidth, MemoryAccess};
use hyper::vm::riscv64::instruction::*;
use hyper::vm::riscv64::time::{TimerWake, timer_wake};

#[test]
fn integer_loads_preserve_signedness_and_x0_destination() {
    for (funct, bytes, signed) in [
        (0, 1, true),
        (1, 2, true),
        (2, 4, true),
        (3, 8, true),
        (4, 1, false),
        (5, 2, false),
        (6, 4, false),
    ] {
        let decoded = crate::require_ok(decode_mmio(
            GuestInstruction::Fetched(0x03 | (funct << 12)),
            MemoryAccess::Read,
        ));
        assert_eq!(decoded.width.bytes(), bytes);
        assert_eq!(decoded.operation, MmioRegister::Load { rd: 0, signed });
        assert_eq!(
            decoded.read_value(u64::MAX),
            if signed || bytes == 8 {
                u64::MAX
            } else {
                (1 << (bytes * 8)) - 1
            }
        );
    }
}

#[test]
fn transformed_compressed_access_keeps_original_pc_length() {
    let word = 0x0000_2181; // Expanded C.LW with rd=3, original length two.
    assert_eq!(
        classify_htinst(word),
        Htinst::Instruction(GuestInstruction::Transformed(word as u32))
    );
    let decoded = crate::require_ok(decode_mmio(
        GuestInstruction::Transformed(word as u32),
        MemoryAccess::Read,
    ));
    assert_eq!(decoded.instruction_bytes, 2);
    assert_eq!(decoded.width, AccessWidth::Word);
    assert_eq!(
        decoded.operation,
        MmioRegister::Load {
            rd: 3,
            signed: true
        }
    );
    let partial = crate::require_ok(decode_mmio(
        GuestInstruction::Transformed(word as u32 | (2 << 15)),
        MemoryAccess::Read,
    ));
    assert_eq!(partial.address_offset, 2);
}

#[test]
fn compressed_register_banks_and_stack_reserved_encodings() {
    for (word, operation) in [
        (
            0x4000,
            MmioRegister::Load {
                rd: 8,
                signed: true,
            },
        ),
        (
            0x601c,
            MmioRegister::Load {
                rd: 15,
                signed: true,
            },
        ),
        (0xc01c, MmioRegister::Store { rs2: 15 }),
        (
            0x4082,
            MmioRegister::Load {
                rd: 1,
                signed: true,
            },
        ),
        (0xe002, MmioRegister::Store { rs2: 0 }),
    ] {
        let access = if matches!(operation, MmioRegister::Load { .. }) {
            MemoryAccess::Read
        } else {
            MemoryAccess::Write
        };
        let decoded = crate::require_ok(decode_mmio(GuestInstruction::Fetched(word), access));
        assert_eq!(decoded.operation, operation);
        assert_eq!(decoded.instruction_bytes, 2);
    }
    assert!(decode_mmio(GuestInstruction::Fetched(0x4002), MemoryAccess::Read).is_err());
    assert!(decode_mmio(GuestInstruction::Fetched(0x6002), MemoryAccess::Read).is_err());
}

#[test]
fn page_walks_unknown_encodings_and_direction_never_become_device_accesses() {
    for value in [0x3000, 0x3020] {
        assert_eq!(classify_htinst(value), Htinst::ImplicitPageWalk);
    }
    assert_eq!(classify_htinst(0), Htinst::Unavailable);
    for value in [0x2000, 0x2002, 0x1_0000_2003] {
        assert_eq!(classify_htinst(value), Htinst::Unsupported);
    }
    for word in [
        0x0000_7023,
        0x0000_7003,
        0x0000_2007,
        0x0000_202f,
        0x0000_001f,
    ] {
        assert!(decode_mmio(GuestInstruction::Fetched(word), MemoryAccess::Read).is_err());
    }
    assert_eq!(
        decode_mmio(GuestInstruction::Fetched(0x2003), MemoryAccess::Write),
        Err(DecodeError::AccessMismatch)
    );
    assert_eq!(
        decode_mmio(GuestInstruction::Fetched(0x2003), MemoryAccess::Execute),
        Err(DecodeError::AccessMismatch)
    );
}

#[test]
fn timer_uses_unsigned_guest_time_and_bounded_rechecks() {
    assert_eq!(timer_wake(100, 10, 110, true), TimerWake::PendingNow);
    assert_eq!(timer_wake(100, 10, 111, true), TimerWake::AfterTicks(1));
    assert_eq!(timer_wake(100, 10, 0, false), TimerWake::Disabled);
    assert_eq!(timer_wake(u64::MAX, 1, 2, true), TimerWake::AfterTicks(2));
    assert_eq!(
        timer_wake(0, u64::MAX, u64::MAX, true),
        TimerWake::PendingNow
    );
    assert_eq!(
        timer_wake(0, 0, u64::MAX, true),
        TimerWake::AfterTicks(i64::MAX as u64)
    );
    assert_eq!(
        timer_wake(u64::MAX - 1, 0, u64::MAX, true),
        TimerWake::AfterTicks(1)
    );
}

#[test]
fn fetched_addresses_reconstruct_split_compressed_offsets_and_negative_immediates() {
    // Encodings independently assembled by LLVM for RV64GC.
    for (word, base, displacement, access) in [
        (0x5de8, 11, 124, MemoryAccess::Read),
        (0x7ef0, 13, 248, MemoryAccess::Read),
        (0xdff8, 15, 124, MemoryAccess::Write),
        (0xfde8, 11, 248, MemoryAccess::Write),
        (0x557e, 2, 252, MemoryAccess::Read),
        (0x75fe, 2, 504, MemoryAccess::Read),
        (0xdfb2, 2, 252, MemoryAccess::Write),
        (0xffb6, 2, 504, MemoryAccess::Write),
        (0xffc5a503, 11, -4, MemoryAccess::Read),
        (0x80c6b023, 13, -2048, MemoryAccess::Write),
    ] {
        let decoded = crate::require_ok(decode_mmio(GuestInstruction::Fetched(word), access));
        assert_eq!(
            decoded.address,
            Some(EffectiveAddress { base, displacement })
        );
    }
}
