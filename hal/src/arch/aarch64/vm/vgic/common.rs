// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Saved virtual CPU-interface state for the `GICv3` TC compatibility trap.
//!
//! These helpers never access physical ICC registers: at EL2 those registers
//! describe the host, not the guest whose bank has just been detached.

use crate::arch::aarch64::vm::vgic::registers;
use registers::SystemRegisterEncoding;

pub(crate) const EXTENDED_INTERRUPT_RANGE: u64 = 1 << 19;

const VMCR_CBPR: u64 = 1 << 4;
const VMCR_EOI_MODE: u64 = 1 << 9;
const VMCR_PMR: u64 = 0xff << 24;

pub(crate) const fn trap_control(control: u64, local_type: u64) -> u64 {
    let trap = if local_type & registers::ICH_VTR_TRAP_DIR_SUPPORTED != 0 {
        registers::ICH_HCR_TRAP_DIR
    } else {
        registers::ICH_HCR_TRAP_COMMON
    };
    // Trap support belongs to the destination CPU, not to the migrated bank.
    (control & !(registers::ICH_HCR_TRAP_DIR | registers::ICH_HCR_TRAP_COMMON))
        | registers::ICH_HCR_ENABLE
        | trap
}

pub(crate) const fn guest_capabilities(value: u64) -> u64 {
    // CTLR PRIbits/IDbits/SEIS/A3V all alias VTR. RSS is not advertised
    // by this platform; ExtRange is captured separately from its physical
    // read-only alias, not inferred from the distributor interrupt count.
    ((value >> 29) & 7) << 8
        | ((value >> 23) & 7) << 11
        | ((value >> 22) & 1) << 14
        | ((value >> 21) & 1) << 15
}

/// Requires a detached, current snapshot of this vCPU's hardware bank.
pub(crate) fn read_common(
    type_register: u64,
    extended_interrupt_range: u64,
    virtual_machine_control: u64,
    active_priorities_group0: &[u64; 4],
    active_priorities_group1: &[u64; 4],
    encoding: SystemRegisterEncoding,
) -> Option<u64> {
    match encoding {
        registers::SYSREG_ICC_PMR_EL1 => Some((virtual_machine_control >> 24) & 0xff),
        registers::SYSREG_ICC_CTLR_EL1 => Some(
            guest_capabilities(type_register)
                | (extended_interrupt_range & EXTENDED_INTERRUPT_RANGE)
                | ((virtual_machine_control & VMCR_CBPR) >> 4)
                | ((virtual_machine_control & VMCR_EOI_MODE) >> 8),
        ),
        registers::SYSREG_ICC_RPR_EL1 => {
            let preemption_bits = ((type_register >> 26) & 7) as u32 + 1;
            let mut priority = 0xff;
            for index in 0..(1 << (preemption_bits - 5)) {
                let active =
                    (active_priorities_group0[index] | active_priorities_group1[index]) as u32;
                if active != 0 {
                    priority = ((index as u32 * 32 + active.trailing_zeros())
                        << (8 - preemption_bits)) as u64;
                    break;
                }
            }
            Some(priority)
        }
        _ => None,
    }
}

/// Updates the saved bank; the next activation installs it in hardware.
pub(crate) fn write_common(
    type_register: u64,
    virtual_machine_control: &mut u64,
    encoding: SystemRegisterEncoding,
    value: u64,
) -> bool {
    match encoding {
        registers::SYSREG_ICC_PMR_EL1 => {
            let priority_bits = ((type_register >> 29) & 7) + 1;
            let priority = value & (0xff << (8 - priority_bits)) & 0xff;
            *virtual_machine_control = (*virtual_machine_control & !VMCR_PMR) | (priority << 24);
            true
        }
        registers::SYSREG_ICC_CTLR_EL1 => {
            *virtual_machine_control = (*virtual_machine_control & !(VMCR_CBPR | VMCR_EOI_MODE))
                | ((value & 1) << 4)
                | ((value & 2) << 8);
            true
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIVE_BITS: u64 = (4 << 29) | (4 << 26);

    #[test]
    fn destination_trap_support_replaces_source_trap_bits() {
        let tc = registers::ICH_HCR_TRAP_COMMON;
        let dir = registers::ICH_HCR_TRAP_DIR;
        let preserved = 1 << 3;
        assert_eq!(trap_control(preserved | dir, 0), preserved | tc | 1);
        assert_eq!(
            trap_control(preserved | tc, registers::ICH_VTR_TRAP_DIR_SUPPORTED),
            preserved | dir | 1
        );
    }

    #[test]
    fn pmr_masks_unimplemented_bits_and_preserves_other_vmcr_fields() {
        for bits in 5..=8 {
            let vtr = (bits - 1) << 29;
            let mut vmcr = 0x5a5a5a;
            assert!(write_common(
                vtr,
                &mut vmcr,
                registers::SYSREG_ICC_PMR_EL1,
                0xff
            ));
            assert_eq!(vmcr & 0xffffff, 0x5a5a5a);
            assert_eq!(
                read_common(
                    vtr,
                    0,
                    vmcr,
                    &[0; 4],
                    &[0; 4],
                    registers::SYSREG_ICC_PMR_EL1
                ),
                Some(0xff & (0xff << (8 - bits)))
            );
        }
    }

    #[test]
    fn ctlr_read_only_capabilities_and_writable_control_are_separate() {
        let vtr = FIVE_BITS | (1 << 23) | (1 << 22) | (1 << 21);
        let mut vmcr = 0xf8 << 24 | 3;
        assert!(write_common(
            vtr,
            &mut vmcr,
            registers::SYSREG_ICC_CTLR_EL1,
            u64::MAX
        ));
        assert_eq!(vmcr, 0xf8 << 24 | 3 | VMCR_CBPR | VMCR_EOI_MODE);
        assert_eq!(
            read_common(
                vtr,
                0,
                vmcr,
                &[0; 4],
                &[0; 4],
                registers::SYSREG_ICC_CTLR_EL1
            ),
            Some(4 << 8 | 1 << 11 | 1 << 14 | 1 << 15 | 3)
        );
        assert!(write_common(
            vtr,
            &mut vmcr,
            registers::SYSREG_ICC_CTLR_EL1,
            0
        ));
        assert_eq!(vmcr, 0xf8 << 24 | 3);
    }

    #[test]
    fn running_priority_uses_both_apr_groups_and_every_implemented_bank() {
        for bits in 5..=7 {
            let vtr = (bits - 1) << 26;
            let count = 1 << (bits - 5);
            assert_eq!(
                read_common(vtr, 0, 0, &[0; 4], &[0; 4], registers::SYSREG_ICC_RPR_EL1),
                Some(0xff)
            );
            for bank in 0..count {
                let mut ap0 = [0; 4];
                let mut ap1 = [0; 4];
                ap0[bank] = 1 << 18;
                ap1[bank] = 1 << 7;
                assert_eq!(
                    read_common(vtr, 0, 0, &ap0, &ap1, registers::SYSREG_ICC_RPR_EL1),
                    Some(((bank as u64 * 32) + 7) << (8 - bits))
                );
            }
        }
    }

    #[test]
    fn extended_range_alias_is_preserved_without_host_control_bits() {
        assert_eq!(
            read_common(
                FIVE_BITS,
                EXTENDED_INTERRUPT_RANGE | 3,
                0,
                &[0; 4],
                &[0; 4],
                registers::SYSREG_ICC_CTLR_EL1
            ),
            Some(guest_capabilities(FIVE_BITS) | EXTENDED_INTERRUPT_RANGE)
        );
    }

    #[test]
    fn read_only_and_unrelated_accesses_are_not_silently_consumed() {
        let mut vmcr = 0;
        assert!(!write_common(
            FIVE_BITS,
            &mut vmcr,
            registers::SYSREG_ICC_RPR_EL1,
            1
        ));
        assert!(!write_common(
            FIVE_BITS,
            &mut vmcr,
            registers::SYSREG_ICC_DIR_EL1,
            33
        ));
        assert_eq!(
            read_common(
                FIVE_BITS,
                0,
                0,
                &[0; 4],
                &[0; 4],
                registers::SYSREG_ICC_DIR_EL1
            ),
            None
        );
    }
}
