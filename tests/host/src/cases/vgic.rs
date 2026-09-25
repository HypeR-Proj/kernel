// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Virtual GIC pending, active, maintenance, and snapshot semantics.

#[allow(dead_code)]
#[path = "../../../../src/vm/arm/gic/ready.rs"]
mod ready_model;

use hyper::vm::arm::gic::lr::{decode as decode_list_register, encode as encode_list_register};
use hyper::vm::arm::gic::{
    BuildError, GicInterruptId, InterruptGroup, InterruptTrigger, ListEntry, ListState,
    RuntimeError, VirtualGicBuilder,
};
use hyper::vm::interrupt::VirtualCpuId;

fn interrupt(value: u32) -> GicInterruptId {
    crate::require_some(GicInterruptId::new(value))
}

#[test]
fn bounded_runtime_storage_rejects_growth_past_its_preallocated_limit() {
    let mut storage = crate::require_ok(ready_model::BoundedVec::try_new(2));
    assert_eq!(storage.push(10), Ok(()));
    assert_eq!(storage.push(20), Ok(()));
    assert_eq!(
        storage.push(30),
        Err(hyper::collections::bounded_vec::Error::Capacity)
    );
    assert_eq!(storage.len(), 2);
    assert_eq!(storage.get(0), Some(&10));
    assert_eq!(storage.get(1), Some(&20));
}

#[test]
fn generic_interrupt_ids_do_not_inherit_gic_intid_limits() {
    assert_eq!(
        hyper::vm::interrupt::VirtualInterruptId::new(u32::MAX).get(),
        u32::MAX
    );
    assert!(GicInterruptId::new(1019).is_some());
    assert!(GicInterruptId::new(1020).is_none());
}

#[test]
fn wfi_wake_query_is_conservative_across_ready_and_resident_states() {
    let cpu = VirtualCpuId::new(0);
    let id = interrupt(48);
    let mut vgic = one_interrupt(48, 1, 1);

    crate::require_ok(vgic.inject(id, cpu));
    assert!(!crate::require_ok(vgic.may_wake_wfi(cpu)));
    crate::require_ok(vgic.set_enabled(id, cpu, true));
    // The saved-state query deliberately ignores priority and group delivery
    // policy. Doing so may over-wake, but can never strand a WFI vCPU behind a
    // policy register which is not yet part of the reusable saved model.
    crate::require_ok(vgic.set_priority(id, cpu, u8::MAX));
    crate::require_ok(vgic.set_group(id, cpu, InterruptGroup::Group0));
    assert!(crate::require_ok(vgic.may_wake_wfi(cpu)));

    let mut slots = [None; 1];
    crate::require_ok(vgic.refill(cpu, &mut slots));
    assert!(crate::require_ok(vgic.may_wake_wfi(cpu)));
    slots[0] = slots[0].map(|mut entry| {
        entry.state = ListState::Active;
        entry
    });
    crate::require_ok(vgic.synchronize(cpu, &slots));
    assert!(!crate::require_ok(vgic.may_wake_wfi(cpu)));

    crate::require_ok(vgic.inject(id, cpu));
    assert!(crate::require_ok(vgic.may_wake_wfi(cpu)));
    crate::require_ok(vgic.clear_pending(id, cpu));
    assert!(!crate::require_ok(vgic.may_wake_wfi(cpu)));
    assert_eq!(
        vgic.may_wake_wfi(VirtualCpuId::new(1)),
        Err(RuntimeError::InvalidCpu)
    );
}

fn one_interrupt(id: u32, cpus: u32, slots: usize) -> hyper::vm::arm::gic::VirtualGic {
    let mut builder = crate::require_ok(VirtualGicBuilder::new(cpus));
    crate::require_ok(builder.configure(
        interrupt(id),
        VirtualCpuId::new(0),
        0x80,
        InterruptGroup::Group1,
        InterruptTrigger::Level,
    ));
    crate::require_ok(builder.finish(slots))
}

#[test]
fn allocation_plan_matches_the_complete_retained_controller_layout() {
    const VCPU_COUNT: u32 = 2;
    const PRIVATE_ENTRIES_PER_VCPU: usize = 32;
    const SHARED_ENTRIES: usize = 32;
    const LIST_REGISTERS: usize = 16;

    let expected = crate::require_ok(hyper::vm::arm::gic::VirtualGic::allocation_requirement(
        VCPU_COUNT,
        PRIVATE_ENTRIES_PER_VCPU,
        SHARED_ENTRIES,
        LIST_REGISTERS,
    ));
    let entry_count = PRIVATE_ENTRIES_PER_VCPU * VCPU_COUNT as usize + SHARED_ENTRIES;
    let mut builder = crate::require_ok(VirtualGicBuilder::new_with_entry_capacity(
        VCPU_COUNT,
        entry_count,
    ));
    for cpu_index in 0..VCPU_COUNT {
        for id in 0..PRIVATE_ENTRIES_PER_VCPU as u32 {
            crate::require_ok(builder.configure(
                interrupt(id),
                VirtualCpuId::new(cpu_index),
                0x80,
                InterruptGroup::Group1,
                InterruptTrigger::Level,
            ));
        }
    }
    for id in 32..(32 + SHARED_ENTRIES as u32) {
        crate::require_ok(builder.configure(
            interrupt(id),
            VirtualCpuId::new(0),
            0x80,
            InterruptGroup::Group1,
            InterruptTrigger::Level,
        ));
    }
    let vgic = crate::require_ok(builder.finish(LIST_REGISTERS));

    assert_eq!(vgic.allocation_size(), Some(expected));
}

#[test]
fn allocation_plan_rejects_unbounded_or_impossible_layouts() {
    use hyper::vm::arm::gic::VirtualGic;

    assert_eq!(
        VirtualGic::allocation_requirement(0, 32, 32, 16),
        Err(BuildError::InvalidStoragePlan)
    );
    assert_eq!(
        VirtualGic::allocation_requirement(1, 33, 32, 16),
        Err(BuildError::InvalidStoragePlan)
    );
    assert_eq!(
        VirtualGic::allocation_requirement(1, 32, 32, 0),
        Err(BuildError::InvalidStoragePlan)
    );

    let mut builder = crate::require_ok(VirtualGicBuilder::new_with_entry_capacity(1, 1));
    for id in 32..=33 {
        let result = builder.configure(
            interrupt(id),
            VirtualCpuId::new(0),
            0x80,
            InterruptGroup::Group1,
            InterruptTrigger::Level,
        );
        if id == 32 {
            crate::require_ok(result);
        } else {
            assert_eq!(result, Err(BuildError::InvalidStoragePlan));
        }
    }
}

#[test]
fn schedules_pending_interrupts_by_priority_and_cpu() {
    let mut builder = crate::require_ok(VirtualGicBuilder::new(2));
    let cpu0 = VirtualCpuId::new(0);
    let cpu1 = VirtualCpuId::new(1);
    for (id, cpu, priority) in [(27, cpu0, 0x80), (27, cpu1, 0x70), (40, cpu0, 0x20)] {
        crate::require_ok(builder.configure(
            interrupt(id),
            cpu,
            priority,
            InterruptGroup::Group1,
            InterruptTrigger::Level,
        ));
    }
    let mut vgic = crate::require_ok(builder.finish(2));
    for (id, cpu) in [(27, cpu0), (27, cpu1), (40, cpu0)] {
        crate::require_ok(vgic.set_enabled(interrupt(id), cpu, true));
        crate::require_ok(vgic.inject(interrupt(id), cpu));
    }

    let mut slots = [None; 2];
    assert_eq!(crate::require_ok(vgic.refill(cpu0, &mut slots)), 2);
    assert_eq!(slots[0].map(|entry| entry.interrupt), Some(interrupt(40)));
    assert_eq!(slots[1].map(|entry| entry.interrupt), Some(interrupt(27)));

    let mut other_slots = [None; 2];
    assert_eq!(crate::require_ok(vgic.refill(cpu1, &mut other_slots)), 1);
    assert_eq!(
        other_slots[0].map(|entry| entry.interrupt),
        Some(interrupt(27))
    );
}

#[test]
fn requests_eoi_maintenance_for_a_virtual_timer_ppi() {
    let mut builder = crate::require_ok(VirtualGicBuilder::new(1));
    let cpu = VirtualCpuId::new(0);
    let timer = interrupt(27);
    crate::require_ok(builder.configure(
        timer,
        cpu,
        0x80,
        InterruptGroup::Group1,
        InterruptTrigger::Level,
    ));
    let mut vgic = crate::require_ok(builder.finish(1));
    crate::require_ok(vgic.set_maintenance_on_eoi(timer, cpu, true));
    crate::require_ok(vgic.set_enabled(timer, cpu, true));
    crate::require_ok(vgic.inject(timer, cpu));
    let mut slots = [None; 1];
    assert_eq!(crate::require_ok(vgic.refill(cpu, &mut slots)), 1);
    assert_eq!(
        slots[0].map(|entry| entry.request_eoi_maintenance),
        Some(true)
    );
}

#[test]
fn tracks_active_reinjection_and_guest_completion() {
    let mut builder = crate::require_ok(VirtualGicBuilder::new(1));
    let cpu = VirtualCpuId::new(0);
    let id = interrupt(48);
    crate::require_ok(builder.configure(
        id,
        cpu,
        0x40,
        InterruptGroup::Group1,
        InterruptTrigger::Level,
    ));
    let mut vgic = crate::require_ok(builder.finish(1));
    crate::require_ok(vgic.set_enabled(id, cpu, true));
    crate::require_ok(vgic.inject(id, cpu));

    let mut slots = [None; 1];
    assert_eq!(crate::require_ok(vgic.refill(cpu, &mut slots)), 1);
    slots[0] = Some(ListEntry {
        source: 0,
        interrupt: id,
        priority: 0x40,
        group: InterruptGroup::Group1,
        state: ListState::Active,
        request_eoi_maintenance: true,
    });
    crate::require_ok(vgic.synchronize(cpu, &slots));
    crate::require_ok(vgic.inject(id, cpu));
    assert_eq!(crate::require_ok(vgic.refill(cpu, &mut slots)), 0);
    assert_eq!(
        slots[0].map(|entry| entry.state),
        Some(ListState::PendingActive)
    );

    crate::require_ok(vgic.synchronize(cpu, &[None]));
    let snapshot = crate::require_ok(vgic.snapshot(id, cpu));
    assert!(!snapshot.pending);
    assert!(!snapshot.active);
    assert!(!snapshot.listed);
}

#[test]
fn withdraws_disabled_pending_entries_without_losing_pending_state() {
    let mut builder = crate::require_ok(VirtualGicBuilder::new(1));
    let cpu = VirtualCpuId::new(0);
    let id = interrupt(72);
    crate::require_ok(builder.configure(
        id,
        cpu,
        0x80,
        InterruptGroup::Group1,
        InterruptTrigger::Level,
    ));
    let mut vgic = crate::require_ok(builder.finish(1));
    crate::require_ok(vgic.set_enabled(id, cpu, true));
    crate::require_ok(vgic.inject(id, cpu));
    let mut slots = [None; 1];
    assert_eq!(crate::require_ok(vgic.refill(cpu, &mut slots)), 1);

    crate::require_ok(vgic.set_enabled(id, cpu, false));
    assert_eq!(crate::require_ok(vgic.refill(cpu, &mut slots)), 0);
    assert_eq!(slots, [None]);
    let snapshot = crate::require_ok(vgic.snapshot(id, cpu));
    assert!(snapshot.pending);
    assert!(!snapshot.listed);

    crate::require_ok(vgic.set_priority(id, cpu, 0x20));
    crate::require_ok(vgic.set_enabled(id, cpu, true));
    assert_eq!(crate::require_ok(vgic.refill(cpu, &mut slots)), 1);
    assert_eq!(slots[0].map(|entry| entry.priority), Some(0x20));
}

#[test]
fn rejects_duplicate_spis_and_malformed_snapshots() {
    let mut builder = crate::require_ok(VirtualGicBuilder::new(2));
    let cpu0 = VirtualCpuId::new(0);
    let cpu1 = VirtualCpuId::new(1);
    let id = interrupt(64);
    crate::require_ok(builder.configure(
        id,
        cpu0,
        0x80,
        InterruptGroup::Group1,
        InterruptTrigger::Edge,
    ));
    assert_eq!(
        builder.configure(
            id,
            cpu1,
            0x80,
            InterruptGroup::Group1,
            InterruptTrigger::Edge,
        ),
        Err(BuildError::AlreadyConfigured)
    );
    let mut vgic = crate::require_ok(builder.finish(2));
    let duplicate = Some(ListEntry {
        source: 0,
        interrupt: id,
        priority: 0x80,
        group: InterruptGroup::Group1,
        state: ListState::Pending,
        request_eoi_maintenance: true,
    });
    assert_eq!(
        vgic.synchronize(cpu0, &[duplicate, duplicate]),
        Err(RuntimeError::SnapshotContainsDuplicate)
    );
}

#[test]
fn encodes_the_gicv3_list_register_layout() {
    let entry = ListEntry {
        source: 0,
        interrupt: interrupt(55),
        priority: 0xa0,
        group: InterruptGroup::Group1,
        state: ListState::PendingActive,
        request_eoi_maintenance: true,
    };
    let encoded = encode_list_register(Some(entry));
    assert_eq!(
        encoded,
        55 | (1 << 41) | (0xa0 << 48) | (1 << 60) | (3 << 62)
    );
    assert_eq!(
        crate::require_ok(decode_list_register(encoded)),
        Some(entry)
    );
    assert_eq!(crate::require_ok(decode_list_register(0)), None);
}

#[test]
fn indexes_private_and_shared_boundary_ids_without_aliasing() {
    let mut builder = crate::require_ok(VirtualGicBuilder::new(2));
    for (id, cpu) in [(0, 0), (31, 0), (0, 1), (31, 1), (32, 0), (1019, 0)] {
        crate::require_ok(builder.configure(
            interrupt(id),
            VirtualCpuId::new(cpu),
            0x80,
            InterruptGroup::Group1,
            InterruptTrigger::Level,
        ));
    }
    let vgic = crate::require_ok(builder.finish(1));
    for (id, cpu) in [(0, 0), (31, 0), (0, 1), (31, 1), (32, 0), (1019, 0)] {
        assert!(vgic.snapshot(interrupt(id), VirtualCpuId::new(cpu)).is_ok());
    }
    assert_eq!(
        vgic.snapshot(interrupt(32), VirtualCpuId::new(1)),
        Err(RuntimeError::NotConfigured)
    );
}

#[test]
fn requires_the_exact_configured_list_register_count() {
    let mut vgic = one_interrupt(40, 1, 2);
    assert_eq!(
        vgic.synchronize(VirtualCpuId::new(0), &[None]),
        Err(RuntimeError::InvalidSlotCount)
    );
    let mut too_many = [None; 3];
    assert_eq!(
        vgic.refill(VirtualCpuId::new(0), &mut too_many),
        Err(RuntimeError::InvalidSlotCount)
    );
}

#[test]
fn reprioritizes_ready_entries_and_breaks_ties_by_intid() {
    let cpu = VirtualCpuId::new(0);
    let mut builder = crate::require_ok(VirtualGicBuilder::new(1));
    for (id, priority) in [(42, 0x80), (40, 0x80), (41, 0x90)] {
        crate::require_ok(builder.configure(
            interrupt(id),
            cpu,
            priority,
            InterruptGroup::Group1,
            InterruptTrigger::Level,
        ));
    }
    let mut vgic = crate::require_ok(builder.finish(3));
    for id in [42, 40, 41] {
        crate::require_ok(vgic.set_enabled(interrupt(id), cpu, true));
        crate::require_ok(vgic.inject(interrupt(id), cpu));
    }
    crate::require_ok(vgic.set_priority(interrupt(41), cpu, 0x70));
    let mut slots = [None; 3];
    assert_eq!(crate::require_ok(vgic.refill(cpu, &mut slots)), 3);
    assert_eq!(
        slots.map(|slot| slot.map(|entry| entry.interrupt)),
        [
            Some(interrupt(41)),
            Some(interrupt(40)),
            Some(interrupt(42))
        ]
    );
}

#[test]
fn injection_and_clear_commands_survive_stale_hardware_snapshots() {
    let cpu = VirtualCpuId::new(0);
    let id = interrupt(48);
    let mut vgic = one_interrupt(48, 1, 1);
    crate::require_ok(vgic.set_enabled(id, cpu, true));
    crate::require_ok(vgic.inject(id, cpu));
    let mut slots = [None; 1];
    crate::require_ok(vgic.refill(cpu, &mut slots));

    slots[0] = slots[0].map(|mut entry| {
        entry.state = ListState::Active;
        entry
    });
    crate::require_ok(vgic.inject(id, cpu));
    crate::require_ok(vgic.synchronize(cpu, &slots));
    crate::require_ok(vgic.refill(cpu, &mut slots));
    assert_eq!(
        slots[0].map(|entry| entry.state),
        Some(ListState::PendingActive)
    );

    crate::require_ok(vgic.clear_pending(id, cpu));
    crate::require_ok(vgic.synchronize(cpu, &slots));
    crate::require_ok(vgic.refill(cpu, &mut slots));
    assert_eq!(slots[0].map(|entry| entry.state), Some(ListState::Active));
    assert!(!crate::require_ok(vgic.snapshot(id, cpu)).pending);
}

#[test]
fn disabled_pending_active_retains_deferred_pending_until_reenabled() {
    let cpu = VirtualCpuId::new(0);
    let id = interrupt(52);
    let mut vgic = one_interrupt(52, 1, 1);
    crate::require_ok(vgic.set_enabled(id, cpu, true));
    crate::require_ok(vgic.inject(id, cpu));
    let mut slots = [None; 1];
    crate::require_ok(vgic.refill(cpu, &mut slots));
    slots[0] = slots[0].map(|mut entry| {
        entry.state = ListState::PendingActive;
        entry
    });
    crate::require_ok(vgic.synchronize(cpu, &slots));
    crate::require_ok(vgic.set_enabled(id, cpu, false));
    crate::require_ok(vgic.refill(cpu, &mut slots));
    assert_eq!(slots[0].map(|entry| entry.state), Some(ListState::Active));
    assert!(crate::require_ok(vgic.snapshot(id, cpu)).pending);

    slots = [None];
    crate::require_ok(vgic.synchronize(cpu, &slots));
    crate::require_ok(vgic.set_enabled(id, cpu, true));
    assert_eq!(crate::require_ok(vgic.refill(cpu, &mut slots)), 1);
    assert_eq!(slots[0].map(|entry| entry.state), Some(ListState::Pending));
}

#[test]
fn routes_ready_and_listed_pending_spi_after_old_bank_withdrawal() {
    let cpu0 = VirtualCpuId::new(0);
    let cpu1 = VirtualCpuId::new(1);
    let id = interrupt(64);
    let mut vgic = one_interrupt(64, 2, 1);
    crate::require_ok(vgic.set_enabled(id, cpu0, true));
    crate::require_ok(vgic.inject(id, cpu0));
    crate::require_ok(vgic.route(id, cpu1));
    let mut old_slots = [None; 1];
    assert_eq!(crate::require_ok(vgic.refill(cpu0, &mut old_slots)), 0);
    let mut new_slots = [None; 1];
    assert_eq!(crate::require_ok(vgic.refill(cpu1, &mut new_slots)), 1);
    crate::require_ok(vgic.route(id, cpu0));
    assert_eq!(crate::require_ok(vgic.snapshot(id, cpu1)).target, cpu1);
    crate::require_ok(vgic.refill(cpu1, &mut new_slots));
    assert_eq!(new_slots, [None]);
    assert_eq!(crate::require_ok(vgic.refill(cpu0, &mut old_slots)), 1);
    assert_eq!(crate::require_ok(vgic.snapshot(id, cpu0)).target, cpu0);
}

#[test]
fn pending_commands_and_disabled_policy_follow_the_complete_lr_matrix() {
    #[derive(Clone, Copy)]
    enum Command {
        None,
        Assert,
        Clear,
    }

    let states = [
        None,
        Some(ListState::Pending),
        Some(ListState::Active),
        Some(ListState::PendingActive),
    ];
    let commands = [Command::None, Command::Assert, Command::Clear];
    for enabled in [false, true] {
        for initial in states {
            for command in commands {
                let cpu = VirtualCpuId::new(0);
                let id = interrupt(80);
                let mut vgic = one_interrupt(80, 1, 1);
                let listed = initial.map(|state| ListEntry {
                    source: 0,
                    interrupt: id,
                    priority: 0x80,
                    group: InterruptGroup::Group1,
                    state,
                    request_eoi_maintenance: true,
                });
                crate::require_ok(vgic.synchronize(cpu, &[listed]));
                crate::require_ok(vgic.set_enabled(id, cpu, enabled));
                match command {
                    Command::None => {}
                    Command::Assert => crate::require_ok(vgic.inject(id, cpu)),
                    Command::Clear => crate::require_ok(vgic.clear_pending(id, cpu)),
                }
                // The hardware snapshot may have been captured before the
                // software command. Synchronization must not consume it.
                crate::require_ok(vgic.synchronize(cpu, &[listed]));
                let mut slots = [listed];
                crate::require_ok(vgic.refill(cpu, &mut slots));

                let after_command = match command {
                    Command::Clear => match initial {
                        Some(ListState::Pending) => None,
                        Some(ListState::PendingActive) => Some(ListState::Active),
                        other => other,
                    },
                    Command::Assert if enabled => match initial {
                        None => Some(ListState::Pending),
                        Some(ListState::Active) => Some(ListState::PendingActive),
                        other => other,
                    },
                    _ => initial,
                };
                let expected = if !enabled && !matches!(command, Command::Clear) {
                    match after_command {
                        Some(ListState::Pending) => None,
                        Some(ListState::PendingActive) => Some(ListState::Active),
                        other => other,
                    }
                } else {
                    after_command
                };
                assert_eq!(slots[0].map(|entry| entry.state), expected);
                let snapshot = crate::require_ok(vgic.snapshot(id, cpu));
                let deferred_by_disable = !enabled
                    && !matches!(command, Command::Clear)
                    && matches!(initial, Some(ListState::Pending | ListState::PendingActive));
                let deferred_assert = !enabled && matches!(command, Command::Assert);
                let expected_pending = matches!(
                    expected,
                    Some(ListState::Pending | ListState::PendingActive)
                ) || deferred_by_disable
                    || deferred_assert;
                assert_eq!(snapshot.pending, expected_pending);
            }
        }
    }
}

#[test]
fn malformed_synchronization_preserves_slots_and_controller_state() {
    let cpu = VirtualCpuId::new(0);
    let id = interrupt(88);
    let mut vgic = one_interrupt(88, 1, 2);
    crate::require_ok(vgic.set_enabled(id, cpu, true));
    crate::require_ok(vgic.inject(id, cpu));
    let mut valid = [None; 2];
    crate::require_ok(vgic.refill(cpu, &mut valid));
    let before = crate::require_ok(vgic.snapshot(id, cpu));
    let malformed = [valid[0], valid[0]];
    assert_eq!(
        vgic.synchronize(cpu, &malformed),
        Err(RuntimeError::SnapshotContainsDuplicate)
    );
    assert_eq!(malformed, [valid[0], valid[0]]);
    assert_eq!(crate::require_ok(vgic.snapshot(id, cpu)), before);
}

#[test]
fn refill_rejects_an_omitted_resident_before_mutating_slots_or_state() {
    let cpu = VirtualCpuId::new(0);
    let mut builder = crate::require_ok(VirtualGicBuilder::new(1));
    for id in [90, 91] {
        crate::require_ok(builder.configure(
            interrupt(id),
            cpu,
            0x80,
            InterruptGroup::Group1,
            InterruptTrigger::Level,
        ));
    }
    let mut vgic = crate::require_ok(builder.finish(2));
    for id in [90, 91] {
        crate::require_ok(vgic.set_enabled(interrupt(id), cpu, true));
        crate::require_ok(vgic.inject(interrupt(id), cpu));
    }
    let mut resident = [None; 2];
    crate::require_ok(vgic.refill(cpu, &mut resident));
    let before_a = crate::require_ok(vgic.snapshot(interrupt(90), cpu));
    let before_b = crate::require_ok(vgic.snapshot(interrupt(91), cpu));
    let mut omitted = [resident[0], None];
    let before_slots = omitted;
    assert_eq!(
        vgic.refill(cpu, &mut omitted),
        Err(RuntimeError::ResidencyMismatch)
    );
    assert_eq!(omitted, before_slots);
    assert_eq!(
        crate::require_ok(vgic.snapshot(interrupt(90), cpu)),
        before_a
    );
    assert_eq!(
        crate::require_ok(vgic.snapshot(interrupt(91), cpu)),
        before_b
    );
}

#[test]
fn clear_normalizes_an_unlisted_disabled_assertion() {
    let cpu = VirtualCpuId::new(0);
    let id = interrupt(92);
    let mut vgic = one_interrupt(92, 1, 1);
    crate::require_ok(vgic.inject(id, cpu));
    crate::require_ok(vgic.clear_pending(id, cpu));
    crate::require_ok(vgic.set_enabled(id, cpu, true));
    let mut slots = [None; 1];
    assert_eq!(crate::require_ok(vgic.refill(cpu, &mut slots)), 0);
    assert_eq!(slots, [None]);
    assert!(!crate::require_ok(vgic.snapshot(id, cpu)).pending);
}

#[test]
fn reserves_worst_case_shared_route_capacity_before_runtime() {
    let cpu0 = VirtualCpuId::new(0);
    let cpu1 = VirtualCpuId::new(1);
    let mut builder = crate::require_ok(VirtualGicBuilder::new(2));
    for id in 32..=1019 {
        crate::require_ok(builder.configure(
            interrupt(id),
            cpu0,
            0x80,
            InterruptGroup::Group1,
            InterruptTrigger::Level,
        ));
    }
    let mut vgic = crate::require_ok(builder.finish(16));
    for id in 32..=1019 {
        crate::require_ok(vgic.set_enabled(interrupt(id), cpu0, true));
        crate::require_ok(vgic.inject(interrupt(id), cpu0));
        crate::require_ok(vgic.route(interrupt(id), cpu1));
    }
    let mut slots = [None; 16];
    assert_eq!(crate::require_ok(vgic.refill(cpu1, &mut slots)), 16);
    for (offset, slot) in slots.iter().enumerate() {
        assert_eq!(
            slot.map(|entry| entry.interrupt),
            Some(interrupt(32 + offset as u32))
        );
    }
}

#[test]
fn boot_prepares_validates_then_commits_interrupt_virtualization() {
    let source = include_str!("../../../../src/kernel/vm/mod.rs");
    let prepare = crate::require_some(source.find("prepare_interrupts(binding.host_interrupt())"));
    let validate = crate::require_some(source.find("timer::validate_hardware("));
    let commit = crate::require_some(source.find("commit_interrupts(prepared_interrupts)"));
    assert!(prepare < validate && validate < commit);

    let controller = include_str!("../../../../hal/src/arch/aarch64/vm_interrupt.rs");
    assert!(!controller.contains("interrupt_virtualization_description"));
    assert!(controller.contains("list_registers: usize"));

    let facade = include_str!("../../../../hal/src/hal/vm.rs");
    let constructor = crate::require_some(facade.find("fn prepare_interrupt_controller("));
    let constructor = &facade[constructor..];
    let constructor_end = crate::require_some(constructor.find("\n}\n"));
    let constructor = &constructor[..constructor_end];
    assert!(constructor.contains("interrupt_virtualization_description()"));
    assert!(constructor.contains("InterruptError::MissingCapabilities"));
}

#[test]
fn live_gic_access_detaches_hardware_around_one_saved_bank_transaction() {
    let source = include_str!("../../../../hal/src/arch/aarch64/vm_vcpu.rs");
    let function = crate::require_some(source.find("pub(crate) fn access_guest_gic("));
    let body = &source[function..];
    let deactivate = crate::require_some(body.find("context.deactivate_vgic()"));
    let transaction = crate::require_some(body.find("interrupts.access_saved_bank("));
    let activate = crate::require_some(body.find("context.activate_vgic()"));
    assert!(deactivate < transaction && transaction < activate);

    let transaction_source = include_str!("../../../../hal/src/arch/aarch64/vm_interrupt.rs");
    let function = crate::require_some(transaction_source.find("fn access_saved_bank("));
    let body = &transaction_source[function..];
    let synchronize = crate::require_some(body.find(".synchronize(vcpu, slots)"));
    let operation = crate::require_some(body.find("match (register, operation)"));
    let refill = crate::require_some(body.find(".refill(vcpu, slots)"));
    assert!(synchronize < operation && operation < refill);
}

#[test]
fn v2_list_registers_preserve_states_and_quantize_priority() {
    use hyper::vm::arm::gic::lr_v2;
    for state in [
        ListState::Pending,
        ListState::Active,
        ListState::PendingActive,
    ] {
        for group in [InterruptGroup::Group0, InterruptGroup::Group1] {
            let entry = ListEntry {
                source: 0,
                interrupt: interrupt(63),
                priority: 0xa7,
                group,
                state,
                request_eoi_maintenance: true,
            };
            let decoded =
                crate::require_some(crate::require_ok(lr_v2::decode(lr_v2::encode(Some(entry)))));
            assert_eq!(
                decoded,
                ListEntry {
                    source: 0,
                    priority: 0xa0,
                    ..entry
                }
            );
        }
    }
    assert_eq!(lr_v2::decode(0), Ok(None));
    assert!(lr_v2::decode((1 << 31) | (1 << 28)).is_err());
}

#[test]
fn distributor_disable_preserves_pending_until_reenabled() {
    let cpu = VirtualCpuId::new(0);
    let mut builder = crate::require_ok(VirtualGicBuilder::new(1));
    crate::require_ok(builder.configure(
        interrupt(32),
        cpu,
        0x80,
        InterruptGroup::Group0,
        InterruptTrigger::Edge,
    ));
    let mut controller = crate::require_ok(builder.finish(64));
    crate::require_ok(controller.set_enabled(interrupt(32), cpu, true));
    crate::require_ok(controller.inject(interrupt(32), cpu));
    let mut slots = [None; 64];
    crate::require_ok(controller.refill(cpu, &mut slots));
    assert!(slots[0].is_some());
    controller.set_distributor_enabled(false);
    crate::require_ok(controller.refill(cpu, &mut slots));
    assert!(slots.iter().all(Option::is_none));
    assert!(!crate::require_ok(controller.may_wake_wfi(cpu)));
    controller.set_distributor_enabled(true);
    crate::require_ok(controller.refill(cpu, &mut slots));
    assert!(slots[0].is_some());
}

#[test]
fn gicv2_sgi_sources_survive_a_full_bank_and_clear_independently() {
    use hyper::vm::arm::gic::lr_v2;
    let cpu = VirtualCpuId::new(0);
    let id = interrupt(3);
    let mut gic = one_interrupt(3, 4, 1);
    crate::require_ok(gic.set_enabled(id, cpu, true));
    crate::require_ok(gic.inject_sgi(id, cpu, VirtualCpuId::new(2)));
    crate::require_ok(gic.inject_sgi(id, cpu, VirtualCpuId::new(3)));
    let mut slots = [None];
    crate::require_ok(gic.refill(cpu, &mut slots));
    assert_eq!(crate::require_some(slots[0]).source, 2);
    assert_eq!(
        crate::require_ok(lr_v2::decode(lr_v2::encode(slots[0]))),
        slots[0]
    );
    // Another source is retained separately while source 2 is active.
    crate::require_some(slots[0].as_mut()).state = ListState::Active;
    crate::require_ok(gic.synchronize(cpu, &slots));
    crate::require_ok(gic.inject_sgi(id, cpu, VirtualCpuId::new(1)));
    crate::require_ok(gic.clear_sgi_sources(id, cpu, 1 << 3));
    crate::require_ok(gic.refill(cpu, &mut slots));
    assert_eq!(crate::require_some(slots[0]).state, ListState::Active);
    assert_eq!(crate::require_ok(gic.sgi_sources(id, cpu)), 1 << 1);
    // Hardware EOI frees the resident source; the pending source is delivered next.
    slots[0] = None;
    crate::require_ok(gic.synchronize(cpu, &slots));
    crate::require_ok(gic.refill(cpu, &mut slots));
    assert_eq!(crate::require_some(slots[0]).source, 1);
    assert_eq!(
        crate::require_ok(lr_v2::decode(lr_v2::encode(slots[0]))),
        slots[0]
    );
}

#[test]
fn active_spi_route_change_keeps_old_owner_until_eoi_and_then_moves_pending() {
    let cpu0 = VirtualCpuId::new(0);
    let cpu1 = VirtualCpuId::new(1);
    let id = interrupt(64);
    let mut gic = one_interrupt(64, 2, 1);
    crate::require_ok(gic.set_enabled(id, cpu0, true));
    crate::require_ok(gic.inject(id, cpu0));
    let mut old = [None];
    let mut new = [None];
    crate::require_ok(gic.refill(cpu0, &mut old));
    crate::require_some(old[0].as_mut()).state = ListState::Active;
    crate::require_ok(gic.synchronize(cpu0, &old));
    crate::require_ok(gic.route(id, cpu1));
    crate::require_ok(gic.inject(id, cpu0));
    crate::require_ok(gic.refill(cpu0, &mut old));
    assert_eq!(crate::require_some(old[0]).state, ListState::Active);
    assert_eq!(crate::require_ok(gic.refill(cpu1, &mut new)), 0);
    assert_eq!(crate::require_ok(gic.target(id)), cpu0);
    old[0] = None;
    crate::require_ok(gic.synchronize(cpu0, &old));
    assert_eq!(crate::require_ok(gic.target(id)), cpu1);
    assert_eq!(crate::require_ok(gic.refill(cpu1, &mut new)), 1);
    assert_eq!(gic.take_reconcile_targets() & 3, 3);
    assert_eq!(gic.take_reconcile_targets(), 0);
}

#[test]
fn vcpu_reset_clears_private_sources_but_retains_pending_shared_delivery() {
    let cpu = VirtualCpuId::new(0);
    let mut builder = crate::require_ok(VirtualGicBuilder::new(2));
    for id in [3, 32] {
        crate::require_ok(builder.configure(
            interrupt(id),
            cpu,
            0x80,
            InterruptGroup::Group0,
            InterruptTrigger::Edge,
        ));
    }
    let mut gic = crate::require_ok(builder.finish(2));
    crate::require_ok(gic.set_enabled(interrupt(3), cpu, true));
    crate::require_ok(gic.set_enabled(interrupt(32), cpu, true));
    crate::require_ok(gic.inject_sgi(interrupt(3), cpu, VirtualCpuId::new(1)));
    crate::require_ok(gic.inject(interrupt(32), cpu));
    let mut slots = [None; 2];
    crate::require_ok(gic.refill(cpu, &mut slots));
    crate::require_ok(gic.reset_vcpu(cpu, InterruptGroup::Group0, true));
    assert!(!crate::require_ok(gic.snapshot(interrupt(3), cpu)).pending);
    assert!(crate::require_ok(gic.snapshot(interrupt(32), cpu)).pending);
    slots = [None; 2];
    assert_eq!(crate::require_ok(gic.refill(cpu, &mut slots)), 1);
    assert_eq!(crate::require_some(slots[0]).interrupt, interrupt(32));
}

#[test]
fn gicv2_masked_sgi_preserves_each_source_and_clearing_resident_keeps_others() {
    let cpu = VirtualCpuId::new(0);
    let id = interrupt(5);
    let mut gic = one_interrupt(5, 4, 1);
    crate::require_ok(gic.set_enabled(id, cpu, true));
    crate::require_ok(gic.inject_sgi(id, cpu, VirtualCpuId::new(2)));
    let mut slots = [None];
    crate::require_ok(gic.refill(cpu, &mut slots));
    gic.set_distributor_enabled(false);
    crate::require_ok(gic.refill(cpu, &mut slots));
    assert_eq!(slots, [None]);
    crate::require_ok(gic.inject_sgi(id, cpu, VirtualCpuId::new(3)));
    assert_eq!(
        crate::require_ok(gic.sgi_sources(id, cpu)),
        (1 << 2) | (1 << 3)
    );
    gic.set_distributor_enabled(true);
    crate::require_ok(gic.refill(cpu, &mut slots));
    assert_eq!(crate::require_some(slots[0]).source, 2);
    crate::require_ok(gic.clear_sgi_sources(id, cpu, 1 << 2));
    crate::require_ok(gic.refill(cpu, &mut slots));
    assert_eq!(crate::require_some(slots[0]).source, 3);
}

#[test]
fn spi_target_mask_selects_one_recipient_and_retains_complete_mask() {
    let id = interrupt(32);
    let cpu0 = VirtualCpuId::new(0);
    let cpu2 = VirtualCpuId::new(2);
    let mut gic = one_interrupt(32, 4, 1);
    crate::require_ok(gic.set_enabled(id, cpu0, true));
    crate::require_ok(gic.inject(id, cpu0));
    crate::require_ok(gic.set_target_mask(id, 0b1100));
    assert_eq!(crate::require_ok(gic.target_mask(id)), 0b1100);
    assert_eq!(crate::require_ok(gic.target(id)), cpu2);
    let mut slots = [None];
    assert_eq!(crate::require_ok(gic.refill(cpu2, &mut slots)), 1);
    let mut other = [None];
    assert_eq!(
        crate::require_ok(gic.refill(VirtualCpuId::new(3), &mut other)),
        0
    );
    crate::require_ok(gic.set_target_mask(id, 0));
    crate::require_ok(gic.refill(cpu2, &mut slots));
    assert_eq!(slots, [None]);
    assert!(crate::require_ok(gic.snapshot(id, cpu2)).pending);
    crate::require_ok(gic.set_target_mask(id, 0b0010));
    assert_eq!(
        crate::require_ok(gic.refill(VirtualCpuId::new(1), &mut other)),
        1
    );
}

#[test]
fn level_line_reasserts_after_eoi_without_repeated_reconcile_prompts() {
    let cpu = VirtualCpuId::new(0);
    let id = interrupt(32);
    let mut gic = one_interrupt(32, 1, 1);
    crate::require_ok(gic.set_enabled(id, cpu, true));
    crate::require_ok(gic.set_line(id, cpu, true));
    assert_eq!(gic.take_reconcile_targets(), 1);
    let mut slots = [None];
    crate::require_ok(gic.refill(cpu, &mut slots));
    for _ in 0..8 {
        crate::require_ok(gic.set_line(id, cpu, true));
        crate::require_ok(gic.synchronize(cpu, &slots));
        crate::require_ok(gic.refill(cpu, &mut slots));
        assert_eq!(gic.take_reconcile_targets(), 0);
    }
    slots[0] = None;
    crate::require_ok(gic.synchronize(cpu, &slots));
    assert_eq!(crate::require_ok(gic.refill(cpu, &mut slots)), 1);
    crate::require_ok(gic.set_line(id, cpu, false));
    assert_eq!(gic.take_reconcile_targets(), 1);
    crate::require_ok(gic.refill(cpu, &mut slots));
    assert_eq!(slots, [None]);
}

#[test]
fn edge_line_latches_a_pulse_and_does_not_reassert_until_the_next_rising_edge() {
    let cpu = VirtualCpuId::new(0);
    let id = interrupt(32);
    let mut gic = one_interrupt(32, 1, 1);
    crate::require_ok(gic.set_trigger(id, cpu, InterruptTrigger::Edge));
    crate::require_ok(gic.set_enabled(id, cpu, true));
    crate::require_ok(gic.set_line(id, cpu, true));
    crate::require_ok(gic.set_line(id, cpu, false));
    let mut slots = [None];
    assert_eq!(crate::require_ok(gic.refill(cpu, &mut slots)), 1);
    slots[0] = None;
    crate::require_ok(gic.synchronize(cpu, &slots));
    assert_eq!(crate::require_ok(gic.refill(cpu, &mut slots)), 0);
    crate::require_ok(gic.set_line(id, cpu, true));
    assert_eq!(crate::require_ok(gic.refill(cpu, &mut slots)), 1);
    slots[0] = None;
    crate::require_ok(gic.synchronize(cpu, &slots));
    assert_eq!(crate::require_ok(gic.refill(cpu, &mut slots)), 0);
    crate::require_ok(gic.set_trigger(id, cpu, InterruptTrigger::Level));
    assert_eq!(crate::require_ok(gic.refill(cpu, &mut slots)), 1);
}

#[test]
fn active_commands_override_stale_lr_snapshots_and_preserve_pending() {
    let cpu = VirtualCpuId::new(0);
    let id = interrupt(48);
    let mut gic = one_interrupt(48, 1, 1);
    crate::require_ok(gic.set_enabled(id, cpu, true));
    crate::require_ok(gic.inject(id, cpu));
    let mut slots = [None];
    crate::require_ok(gic.refill(cpu, &mut slots));
    crate::require_ok(gic.set_active(id, cpu, true));
    crate::require_ok(gic.synchronize(cpu, &slots));
    assert!(crate::require_ok(gic.snapshot(id, cpu)).active);
    crate::require_ok(gic.refill(cpu, &mut slots));
    assert_eq!(
        crate::require_some(slots[0]).state,
        ListState::PendingActive
    );
    crate::require_ok(gic.set_active(id, cpu, false));
    crate::require_ok(gic.synchronize(cpu, &slots));
    assert!(!crate::require_ok(gic.snapshot(id, cpu)).active);
    assert!(crate::require_ok(gic.snapshot(id, cpu)).pending);
    crate::require_ok(gic.refill(cpu, &mut slots));
    assert_eq!(crate::require_some(slots[0]).state, ListState::Pending);
}

#[test]
fn active_command_survives_concurrent_hardware_deactivation() {
    let cpu = VirtualCpuId::new(0);
    let id = interrupt(48);
    let mut gic = one_interrupt(48, 1, 1);
    crate::require_ok(gic.set_enabled(id, cpu, true));
    crate::require_ok(gic.inject(id, cpu));
    let mut slots = [None];
    crate::require_ok(gic.refill(cpu, &mut slots));
    crate::require_ok(gic.set_active(id, cpu, true));
    slots[0] = None;
    crate::require_ok(gic.synchronize(cpu, &slots));
    assert!(crate::require_ok(gic.snapshot(id, cpu)).active);
    crate::require_ok(gic.set_active(id, cpu, false));
    assert!(!crate::require_ok(gic.snapshot(id, cpu)).active);
}

#[test]
fn clearing_active_level_irq_reasserts_line_and_obeys_disabled_state() {
    let cpu = VirtualCpuId::new(0);
    let id = interrupt(48);
    let mut gic = one_interrupt(48, 1, 1);
    crate::require_ok(gic.set_active(id, cpu, true));
    crate::require_ok(gic.set_line(id, cpu, true));
    crate::require_ok(gic.clear_pending(id, cpu));
    crate::require_ok(gic.set_active(id, cpu, false));
    assert!(crate::require_ok(gic.snapshot(id, cpu)).pending);
    let mut slots = [None];
    assert_eq!(crate::require_ok(gic.refill(cpu, &mut slots)), 0);
    crate::require_ok(gic.set_enabled(id, cpu, true));
    assert_eq!(crate::require_ok(gic.refill(cpu, &mut slots)), 1);
}

#[test]
fn software_active_preempts_pending_residency_without_losing_pending() {
    let cpu = VirtualCpuId::new(0);
    let mut builder = crate::require_ok(VirtualGicBuilder::new(1));
    for id in [48, 49, 50] {
        crate::require_ok(builder.configure(
            interrupt(id),
            cpu,
            0x80,
            InterruptGroup::Group1,
            InterruptTrigger::Edge,
        ));
    }
    let mut gic = crate::require_ok(builder.finish(1));
    crate::require_ok(gic.set_enabled(interrupt(48), cpu, true));
    crate::require_ok(gic.inject(interrupt(48), cpu));
    let mut slots = [None];
    crate::require_ok(gic.refill(cpu, &mut slots));
    // Active state needs hardware residency even while disabled.
    crate::require_ok(gic.set_active(interrupt(49), cpu, true));
    crate::require_ok(gic.set_active(interrupt(50), cpu, true));
    crate::require_ok(gic.refill(cpu, &mut slots));
    assert_eq!(crate::require_some(slots[0]).interrupt, interrupt(49));
    assert_eq!(crate::require_some(slots[0]).state, ListState::Active);
    assert!(crate::require_ok(gic.snapshot(interrupt(48), cpu)).pending);
    let overflow = crate::require_ok(gic.snapshot(interrupt(50), cpu));
    assert!(overflow.active);
    assert!(!overflow.listed);
    crate::require_ok(gic.set_active(interrupt(49), cpu, false));
    crate::require_ok(gic.refill(cpu, &mut slots));
    assert_eq!(crate::require_some(slots[0]).interrupt, interrupt(50));
    // A hardware deactivate of a resident software-set active IRQ clears it.
    slots[0] = None;
    crate::require_ok(gic.synchronize(cpu, &slots));
    assert!(!crate::require_ok(gic.snapshot(interrupt(50), cpu)).active);
    crate::require_ok(gic.refill(cpu, &mut slots));
    assert_eq!(crate::require_some(slots[0]).interrupt, interrupt(48));
}

#[test]
fn active_route_is_held_until_deactivation_and_reset_releases_overflow() {
    let cpu = VirtualCpuId::new(0);
    let other = VirtualCpuId::new(1);
    let mut builder = crate::require_ok(VirtualGicBuilder::new(2));
    for id in [48, 49] {
        crate::require_ok(builder.configure(
            interrupt(id),
            cpu,
            0x80,
            InterruptGroup::Group1,
            InterruptTrigger::Edge,
        ));
    }
    let mut gic = crate::require_ok(builder.finish(1));
    for id in [48, 49] {
        crate::require_ok(gic.set_active(interrupt(id), cpu, true));
        crate::require_ok(gic.route(interrupt(id), other));
        assert_eq!(crate::require_ok(gic.target(interrupt(id))), cpu);
    }
    let mut slots = [None];
    crate::require_ok(gic.refill(cpu, &mut slots));
    assert!(crate::require_ok(gic.snapshot(interrupt(49), cpu)).active);
    crate::require_ok(gic.reset_vcpu(cpu, InterruptGroup::Group1, false));
    for id in [48, 49] {
        assert_eq!(crate::require_ok(gic.target(interrupt(id))), other);
        assert!(!crate::require_ok(gic.snapshot(interrupt(id), other)).active);
    }
}

#[test]
fn active_admission_ignores_distributor_enable_but_pending_does_not() {
    let cpu = VirtualCpuId::new(0);
    let id = interrupt(48);
    let mut gic = one_interrupt(48, 1, 1);
    crate::require_ok(gic.set_enabled(id, cpu, true));
    crate::require_ok(gic.inject(id, cpu));
    crate::require_ok(gic.set_active(id, cpu, true));
    gic.set_distributor_enabled(false);
    let mut slots = [None];
    crate::require_ok(gic.refill(cpu, &mut slots));
    assert_eq!(crate::require_some(slots[0]).state, ListState::Active);
    assert!(crate::require_ok(gic.snapshot(id, cpu)).pending);
    gic.set_distributor_enabled(true);
    crate::require_ok(gic.refill(cpu, &mut slots));
    assert_eq!(
        crate::require_some(slots[0]).state,
        ListState::PendingActive
    );
}

#[test]
fn active_bitmap_mmio_reads_aliases_and_writes_only_selected_bits() {
    use hyper::vm::arm::gic::mmio::{
        DISTRIBUTOR_BASE, DecodedRegister, decode_v2, read_model_register, write_model_register,
    };
    use hyper::vm::exit::{AccessWidth, GuestPhysicalAddress};
    let cpu = VirtualCpuId::new(0);
    let mut builder = crate::require_ok(VirtualGicBuilder::new(1));
    for id in 32..64 {
        crate::require_ok(builder.configure(
            interrupt(id),
            cpu,
            0x80,
            InterruptGroup::Group1,
            InterruptTrigger::Edge,
        ));
    }
    let mut gic = crate::require_ok(builder.finish(1));
    let register = |offset| {
        let decoded = crate::require_some(crate::require_ok(decode_v2(
            GuestPhysicalAddress::new(u64::from(DISTRIBUTOR_BASE) + offset),
            AccessWidth::Word,
        )));
        let DecodedRegister::Model(register) = decoded.register() else {
            unreachable!()
        };
        register
    };
    let set = register(0x304);
    let clear = register(0x384);
    crate::require_ok(write_model_register(&mut gic, cpu, set, 0b101));
    assert_eq!(
        crate::require_ok(read_model_register(&gic, cpu, set)),
        0b101
    );
    assert_eq!(
        crate::require_ok(read_model_register(&gic, cpu, clear)),
        0b101
    );
    crate::require_ok(write_model_register(&mut gic, cpu, clear, 1));
    assert_eq!(
        crate::require_ok(read_model_register(&gic, cpu, set)),
        0b100
    );
    crate::require_ok(write_model_register(&mut gic, cpu, clear, 0));
    assert_eq!(
        crate::require_ok(read_model_register(&gic, cpu, set)),
        0b100
    );
}

#[test]
fn active_pressure_preserves_evicted_sgi_source() {
    let cpu = VirtualCpuId::new(0);
    let source = VirtualCpuId::new(2);
    let mut builder = crate::require_ok(VirtualGicBuilder::new(3));
    for id in [3, 48] {
        crate::require_ok(builder.configure(
            interrupt(id),
            cpu,
            0x80,
            InterruptGroup::Group1,
            InterruptTrigger::Edge,
        ));
    }
    let mut gic = crate::require_ok(builder.finish(1));
    crate::require_ok(gic.set_enabled(interrupt(3), cpu, true));
    crate::require_ok(gic.inject_sgi(interrupt(3), cpu, source));
    let mut slots = [None];
    crate::require_ok(gic.refill(cpu, &mut slots));
    assert_eq!(crate::require_some(slots[0]).source, 2);
    crate::require_ok(gic.set_active(interrupt(48), cpu, true));
    crate::require_ok(gic.refill(cpu, &mut slots));
    assert_eq!(crate::require_some(slots[0]).interrupt, interrupt(48));
    crate::require_ok(gic.set_active(interrupt(48), cpu, false));
    crate::require_ok(gic.refill(cpu, &mut slots));
    assert_eq!(crate::require_some(slots[0]).interrupt, interrupt(3));
    assert_eq!(crate::require_some(slots[0]).source, 2);
}

#[test]
fn dir_deactivates_overflow_in_any_order_and_obeys_eoi_mode() {
    let cpu = VirtualCpuId::new(0);
    let mut builder = crate::require_ok(VirtualGicBuilder::new(1));
    for id in 32..48 {
        crate::require_ok(builder.configure(
            interrupt(id),
            cpu,
            0x80,
            InterruptGroup::Group1,
            InterruptTrigger::Edge,
        ));
    }
    let mut model = crate::require_ok(builder.finish(2));
    for id in 32..48 {
        crate::require_ok(model.set_active(interrupt(id), cpu, true));
    }
    let mut slots = [None; 2];
    crate::require_ok(model.refill(cpu, &mut slots));
    crate::require_ok(model.deactivate(cpu, 47, None, false));
    assert!(crate::require_ok(model.snapshot(interrupt(47), cpu)).active);
    for invalid in [48, 1019, 1020, 1023, u32::MAX] {
        crate::require_ok(model.deactivate(cpu, invalid, None, true));
    }
    for id in [
        47, 32, 45, 34, 43, 36, 41, 38, 39, 40, 37, 42, 35, 44, 33, 46,
    ] {
        crate::require_ok(model.deactivate(cpu, id, None, true));
        crate::require_ok(model.refill(cpu, &mut slots));
        assert!(!crate::require_ok(model.snapshot(interrupt(id), cpu)).active);
    }
    assert!(slots.iter().all(Option::is_none));
}

#[test]
fn dir_preserves_pending_and_requires_the_active_sgi_source() {
    let cpu = VirtualCpuId::new(0);
    let id = interrupt(3);
    let mut model = one_interrupt(3, 3, 1);
    crate::require_ok(model.set_enabled(id, cpu, true));
    crate::require_ok(model.inject_sgi(id, cpu, VirtualCpuId::new(1)));
    let mut slots = [None];
    crate::require_ok(model.refill(cpu, &mut slots));
    let slot = crate::require_some(slots[0].as_mut());
    slot.state = ListState::Active;
    crate::require_ok(model.synchronize(cpu, &slots));
    crate::require_ok(model.inject_sgi(id, cpu, VirtualCpuId::new(2)));
    crate::require_ok(model.refill(cpu, &mut slots));
    assert_eq!(crate::require_some(slots[0]).state, ListState::Active);
    crate::require_ok(model.deactivate(cpu, 3, Some(2), true));
    assert!(crate::require_ok(model.snapshot(id, cpu)).active);
    crate::require_ok(model.deactivate(cpu, 3, Some(1), true));
    crate::require_ok(model.refill(cpu, &mut slots));
    let slot = crate::require_some(slots[0]);
    assert_eq!(slot.source, 2);
    assert_eq!(slot.state, ListState::Pending);
}

#[test]
fn software_active_sgi_keeps_canonical_source_while_another_source_waits() {
    let cpu = VirtualCpuId::new(0);
    let id = interrupt(3);
    let mut model = one_interrupt(3, 2, 1);
    crate::require_ok(model.set_enabled(id, cpu, true));
    crate::require_ok(model.set_active(id, cpu, true));
    crate::require_ok(model.inject_sgi(id, cpu, VirtualCpuId::new(1)));
    let mut slots = [None];
    crate::require_ok(model.refill(cpu, &mut slots));
    let slot = crate::require_some(slots[0]);
    assert_eq!(slot.source, 0);
    assert_eq!(slot.state, ListState::Active);
    crate::require_ok(model.deactivate(cpu, 3, Some(1), true));
    assert!(crate::require_ok(model.snapshot(id, cpu)).active);
    crate::require_ok(model.deactivate(cpu, 3, Some(0), true));
    crate::require_ok(model.refill(cpu, &mut slots));
    assert_eq!(crate::require_some(slots[0]).source, 1);
    assert_eq!(crate::require_some(slots[0]).state, ListState::Pending);
}

#[test]
fn folding_pending_sgi_into_active_lr_consumes_only_its_source() {
    let cpu = VirtualCpuId::new(0);
    let id = interrupt(3);
    let mut model = one_interrupt(3, 3, 1);
    crate::require_ok(model.set_enabled(id, cpu, true));
    crate::require_ok(model.inject_sgi(id, cpu, VirtualCpuId::new(1)));
    let mut slots = [None];
    crate::require_ok(model.refill(cpu, &mut slots));
    crate::require_some(slots[0].as_mut()).state = ListState::Active;
    crate::require_ok(model.synchronize(cpu, &slots));
    crate::require_ok(model.inject_sgi(id, cpu, VirtualCpuId::new(1)));
    crate::require_ok(model.inject_sgi(id, cpu, VirtualCpuId::new(2)));
    // An ISPENDR write also requests pending on the currently resident instance.
    crate::require_ok(model.inject(id, cpu));
    crate::require_ok(model.refill(cpu, &mut slots));
    assert_eq!(
        crate::require_some(slots[0]).state,
        ListState::PendingActive
    );
    crate::require_ok(model.deactivate(cpu, 3, Some(1), true));
    crate::require_ok(model.refill(cpu, &mut slots));
    assert_eq!(crate::require_some(slots[0]).state, ListState::Pending);
    // Guest acknowledges and completes the re-pended A instance.
    slots[0] = None;
    crate::require_ok(model.synchronize(cpu, &slots));
    crate::require_ok(model.refill(cpu, &mut slots));
    assert_eq!(crate::require_some(slots[0]).source, 2);
    slots[0] = None;
    crate::require_ok(model.synchronize(cpu, &slots));
    crate::require_ok(model.refill(cpu, &mut slots));
    assert!(slots[0].is_none());
}
