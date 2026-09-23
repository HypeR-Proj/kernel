// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

use hyper::vm::arm::gic::mmio::{DISTRIBUTOR_BASE, DecodedRegister, ModelRegister, decode_v3_cpus};
use hyper::vm::arm::gic::quiesce::ActiveAccessQuiesce;
use hyper::vm::arm::gic::{
    GicInterruptId, InterruptGroup, InterruptTrigger, ListState, VirtualGic, VirtualGicBuilder,
};
use hyper::vm::exit::{AccessWidth, GuestPhysicalAddress, MmioOperation};
use hyper::vm::interrupt::VirtualCpuId;

fn cpu(index: u32) -> VirtualCpuId {
    VirtualCpuId::new(index)
}
fn interrupt() -> GicInterruptId {
    crate::require_some(GicInterruptId::new(48))
}
fn model() -> VirtualGic {
    let mut builder = crate::require_ok(VirtualGicBuilder::new(3));
    for id in 32..64 {
        crate::require_ok(builder.configure(
            crate::require_some(GicInterruptId::new(id)),
            cpu(1),
            0x80,
            InterruptGroup::Group1,
            InterruptTrigger::Edge,
        ));
    }
    let mut controller = crate::require_ok(builder.finish(2));
    crate::require_ok(controller.set_enabled(interrupt(), cpu(1), true));
    controller
}
fn register(offset: u64) -> ModelRegister {
    let access = crate::require_some(crate::require_ok(decode_v3_cpus(
        GuestPhysicalAddress::new(u64::from(DISTRIBUTOR_BASE) + offset),
        AccessWidth::Word,
        3,
    )));
    match access.register() {
        DecodedRegister::Model(register) => register,
        _ => panic!("expected active bitmap"),
    }
}

#[test]
fn active_read_waits_for_fresh_remote_lr_and_excludes_new_entry() {
    let mut model = model();
    let mut gate = crate::require_ok(ActiveAccessQuiesce::try_new(3));
    assert!(gate.try_enter(cpu(0)));
    assert!(gate.try_enter(cpu(1)));
    crate::require_ok(model.inject(interrupt(), cpu(1)));
    let mut slots = [None; 2];
    crate::require_ok(model.refill(cpu(1), &mut slots));
    // The guest has acknowledged its interrupt but the model still says pending.
    slots[0] = slots[0].map(|mut entry| {
        entry.state = ListState::Active;
        entry
    });
    crate::require_ok(gate.begin(cpu(0), cpu(0), register(0x304), MmioOperation::Read));
    assert_eq!(gate.take_prompts(), 3);
    assert!(!gate.try_enter(cpu(2)));
    gate.bank_saved(cpu(0), &mut model);
    assert!(gate.take(cpu(0)).is_none());
    crate::require_ok(model.synchronize(cpu(1), &slots));
    gate.bank_saved(cpu(1), &mut model);
    assert_eq!(gate.take(cpu(0)), Some(Ok(Some(1 << 16))));
    assert!(!gate.gate_closed());
    assert_eq!(gate.take_prompts(), 7);
    assert!(gate.try_enter(cpu(2)));
    gate.bank_saved(cpu(2), &mut model);
}

#[test]
fn concurrent_callers_detach_without_owning_each_others_gate() {
    let mut model = model();
    let mut gate = crate::require_ok(ActiveAccessQuiesce::try_new(3));
    assert!(gate.try_enter(cpu(0)));
    assert!(gate.try_enter(cpu(1)));
    crate::require_ok(gate.begin(
        cpu(0),
        cpu(0),
        register(0x304),
        MmioOperation::Write(1 << 16),
    ));
    crate::require_ok(gate.begin(cpu(1), cpu(1), register(0x384), MmioOperation::Read));
    gate.bank_saved(cpu(1), &mut model);
    assert!(gate.take(cpu(1)).is_none());
    gate.bank_saved(cpu(0), &mut model);
    assert_eq!(gate.take(cpu(0)), Some(Ok(None)));
    assert_eq!(gate.take(cpu(1)), Some(Ok(Some(1 << 16))));
    assert!(!gate.gate_closed());
}

#[test]
fn inactive_peers_do_not_prevent_single_cpu_overcommit_progress() {
    let mut model = model();
    let mut gate = crate::require_ok(ActiveAccessQuiesce::try_new(3));
    assert!(gate.try_enter(cpu(0)));
    crate::require_ok(gate.begin(cpu(0), cpu(0), register(0x304), MmioOperation::Read));
    gate.bank_saved(cpu(0), &mut model);
    assert_eq!(gate.take(cpu(0)), Some(Ok(Some(0))));
    assert!(gate.try_enter(cpu(1)));
    gate.bank_saved(cpu(1), &mut model);
}

#[test]
fn cancellation_reopens_gate_and_does_not_execute_cancelled_write() {
    let mut model = model();
    let mut gate = crate::require_ok(ActiveAccessQuiesce::try_new(3));
    assert!(gate.try_enter(cpu(0)));
    assert!(gate.try_enter(cpu(1)));
    crate::require_ok(gate.begin(
        cpu(0),
        cpu(0),
        register(0x304),
        MmioOperation::Write(1 << 16),
    ));
    gate.bank_saved(cpu(0), &mut model);
    gate.cancel(cpu(0), &mut model);
    assert!(!gate.pending(cpu(0)));
    assert!(!gate.gate_closed());
    assert_eq!(gate.take_prompts(), 7);
    gate.bank_saved(cpu(1), &mut model);
    assert!(!crate::require_ok(model.snapshot(interrupt(), cpu(1))).active);
}

#[test]
fn failed_activation_withdrawal_completes_waiter_and_reuses_slot() {
    let mut model = model();
    let mut gate = crate::require_ok(ActiveAccessQuiesce::try_new(3));
    assert!(gate.try_enter(cpu(0)));
    assert!(gate.try_enter(cpu(1)));
    crate::require_ok(gate.begin(cpu(0), cpu(0), register(0x304), MmioOperation::Read));
    assert!(
        gate.begin(cpu(0), cpu(0), register(0x304), MmioOperation::Read)
            .is_err()
    );
    gate.bank_saved(cpu(0), &mut model);
    gate.bank_saved(cpu(1), &mut model);
    assert_eq!(gate.take(cpu(0)), Some(Ok(Some(0))));
    assert!(gate.try_enter(cpu(0)));
    crate::require_ok(gate.begin(cpu(0), cpu(0), register(0x304), MmioOperation::Read));
    gate.bank_saved(cpu(0), &mut model);
    gate.cancel(cpu(0), &mut model);
    assert!(!gate.pending(cpu(0)));
}

#[test]
fn remote_redistributor_access_uses_the_addressed_private_bank() {
    use hyper::vm::arm::gic::mmio::REDISTRIBUTOR_BASE;
    let mut builder = crate::require_ok(VirtualGicBuilder::new(2));
    for bank in 0..2 {
        for id in 0..32 {
            crate::require_ok(builder.configure(
                crate::require_some(GicInterruptId::new(id)),
                cpu(bank),
                0x80,
                InterruptGroup::Group1,
                InterruptTrigger::Edge,
            ));
        }
    }
    let mut model = crate::require_ok(builder.finish(1));
    let mut gate = crate::require_ok(ActiveAccessQuiesce::try_new(2));
    let id = crate::require_some(GicInterruptId::new(27));
    crate::require_ok(model.set_active(id, cpu(1), true));
    let access = crate::require_some(crate::require_ok(decode_v3_cpus(
        GuestPhysicalAddress::new(u64::from(REDISTRIBUTOR_BASE) + 0x30300),
        AccessWidth::Word,
        2,
    )));
    assert_eq!(access.redistributor(), Some(1));
    let DecodedRegister::Model(register) = access.register() else {
        panic!("expected private active bitmap");
    };
    assert!(gate.try_enter(cpu(0)));
    crate::require_ok(gate.begin(cpu(0), cpu(1), register, MmioOperation::Read));
    gate.bank_saved(cpu(0), &mut model);
    assert_eq!(gate.take(cpu(0)), Some(Ok(Some(1 << 27))));
    assert!(!crate::require_ok(model.snapshot(id, cpu(0))).active);
}

#[test]
fn remote_dir_waits_for_saved_lr_and_preserves_pending() {
    let mut model = model();
    let mut gate = crate::require_ok(ActiveAccessQuiesce::try_new(3));
    assert!(gate.try_enter(cpu(0)));
    assert!(gate.try_enter(cpu(1)));
    crate::require_ok(model.inject(interrupt(), cpu(1)));
    let mut slots = [None; 2];
    crate::require_ok(model.refill(cpu(1), &mut slots));
    crate::require_some(slots[0].as_mut()).state = ListState::PendingActive;
    crate::require_ok(gate.begin_deactivate(cpu(0), 48, None, true));
    gate.bank_saved(cpu(0), &mut model);
    assert!(gate.take(cpu(0)).is_none());
    crate::require_ok(model.synchronize(cpu(1), &slots));
    gate.bank_saved(cpu(1), &mut model);
    assert_eq!(gate.take(cpu(0)), Some(Ok(None)));
    crate::require_ok(model.refill(cpu(1), &mut slots));
    assert_eq!(crate::require_some(slots[0]).state, ListState::Pending);
    assert!(!crate::require_ok(model.snapshot(interrupt(), cpu(1))).active);
}

#[test]
fn gicv2_dir_page_is_emulated_but_iar_eoir_page_remains_direct() {
    use hyper::vm::arm::gic::mmio::{Frame, ServiceRegister, decode_v2};
    let base = 0x0801_0000;
    assert_eq!(
        crate::require_ok(decode_v2(
            GuestPhysicalAddress::new(base + 0xc),
            AccessWidth::Word
        )),
        None
    );
    let dir = crate::require_some(crate::require_ok(decode_v2(
        GuestPhysicalAddress::new(base + 0x1000),
        AccessWidth::Word,
    )));
    assert_eq!(dir.frame(), Frame::CpuDeactivateV2);
    assert_eq!(
        dir.register(),
        DecodedRegister::Service(ServiceRegister::CpuDeactivateV2)
    );
    assert!(
        decode_v2(
            GuestPhysicalAddress::new(base + 0x1000),
            AccessWidth::DoubleWord
        )
        .is_err()
    );
    assert!(decode_v2(GuestPhysicalAddress::new(base + 0x1001), AccessWidth::Word).is_err());
    let reserved = crate::require_some(crate::require_ok(decode_v2(
        GuestPhysicalAddress::new(base + 0x1004),
        AccessWidth::Word,
    )));
    assert_eq!(reserved.register(), DecodedRegister::Reserved);
}
