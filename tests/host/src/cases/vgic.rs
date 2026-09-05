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
    assert_eq!(storage.push(30), Err(ready_model::ReadyError::Capacity));
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
fn routes_a_ready_spi_atomically_and_rejects_a_listed_route() {
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
    assert_eq!(vgic.route(id, cpu0), Err(RuntimeError::Busy));
    assert_eq!(crate::require_ok(vgic.snapshot(id, cpu1)).target, cpu1);
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

    let controller = include_str!("../../../../src/arch/aarch64/vm_interrupt.rs");
    assert!(!controller.contains("interrupt_virtualization_description"));
    assert!(controller.contains("list_registers: usize"));

    let facade = include_str!("../../../../src/hal/selected/vm.rs");
    let constructor = crate::require_some(facade.find("fn create_interrupt_controller("));
    let constructor = &facade[constructor..];
    let constructor_end = crate::require_some(constructor.find("\n}\n"));
    let constructor = &constructor[..constructor_end];
    assert!(constructor.contains("interrupt_virtualization_description()"));
    assert!(constructor.contains("InterruptError::MissingCapabilities"));

    let linux = include_str!("../../../../src/kernel/vm/linux/mod.rs");
    let prepare = crate::require_some(linux.find("fn prepare_boot_vcpu("));
    let prepare = &linux[prepare..];
    let prepare_end = crate::require_some(prepare.find("\n}\n"));
    let prepare = &prepare[..prepare_end];
    assert!(!prepare.contains("interrupt_virtualization_description"));
}

#[test]
fn live_gic_access_detaches_hardware_around_one_saved_bank_transaction() {
    let source = include_str!("../../../../src/arch/aarch64/vm_vcpu.rs");
    let function = crate::require_some(source.find("pub(crate) fn access_guest_gic("));
    let body = &source[function..];
    let deactivate = crate::require_some(body.find("context.deactivate_vgic()"));
    let transaction = crate::require_some(body.find("interrupts.access_saved_bank("));
    let activate = crate::require_some(body.find("context.activate_vgic()"));
    assert!(deactivate < transaction && transaction < activate);

    let transaction_source = include_str!("../../../../src/arch/aarch64/vm_interrupt.rs");
    let function = crate::require_some(transaction_source.find("fn access_saved_bank("));
    let body = &transaction_source[function..];
    let synchronize = crate::require_some(body.find(".synchronize(vcpu, slots)"));
    let operation = crate::require_some(body.find("match (register, operation)"));
    let refill = crate::require_some(body.find(".refill(vcpu, slots)"));
    assert!(synchronize < operation && operation < refill);
}
