// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

use hyper::vm::arm::gic::mmio::{
    BitmapRegister, DISTRIBUTOR_BASE, DISTRIBUTOR_SIZE, DecodeError, DecodedRegister, Frame,
    INTERRUPT_COUNT, ModelRegister, ModelRegisterDescriptor, REDISTRIBUTOR_BASE,
    REDISTRIBUTOR_SIZE, RegisterState, ServiceRegister, decode_v3, read_model_register,
    write_model_register,
};
use hyper::vm::arm::gic::{
    GicInterruptId, InterruptGroup, InterruptTrigger, VirtualGic, VirtualGicBuilder,
};
use hyper::vm::exit::{AccessWidth, GuestPhysicalAddress};
use hyper::vm::interrupt::VirtualCpuId;

fn decode(address: u64, width: AccessWidth) -> Result<Option<DecodedRegister>, DecodeError> {
    decode_v3(GuestPhysicalAddress::new(address), width)
        .map(|access| access.map(|decoded| decoded.register()))
}

fn register(address: u64, width: AccessWidth) -> DecodedRegister {
    crate::require_some(crate::require_ok(decode(address, width)))
}

fn model_register(address: u64, width: AccessWidth) -> ModelRegister {
    match register(address, width) {
        DecodedRegister::Model(register) => register,
        other => panic!("expected model register, received {other:?}"),
    }
}

fn interrupt(id: u32) -> GicInterruptId {
    crate::require_some(GicInterruptId::new(id))
}

fn controller() -> VirtualGic {
    let mut controller = crate::require_ok(VirtualGicBuilder::new(1));
    for id in 32..INTERRUPT_COUNT {
        crate::require_ok(controller.configure(
            interrupt(id),
            VirtualCpuId::new(0),
            0x80,
            InterruptGroup::Group1,
            InterruptTrigger::Level,
        ));
    }
    crate::require_ok(controller.finish(1))
}

#[test]
fn every_advertised_spi_bank_supports_configuration_and_delivery() {
    use hyper::vm::arm::gic::mmio::decode_v2;
    assert_eq!(INTERRUPT_COUNT, 256);
    let registers = RegisterState::new();
    for service in [
        ServiceRegister::DistributorTypeV2,
        ServiceRegister::DistributorType,
    ] {
        assert_eq!(
            (registers.read(service) & 31) + 1,
            u64::from(INTERRUPT_COUNT / 32)
        );
    }
    for v2 in [false, true] {
        for id in [40, 63, 64, 95, 96, 127, 128, 191, 192, 255] {
            let decode = |offset: u64, width| {
                let address = u64::from(DISTRIBUTOR_BASE) + offset;
                let register = if v2 {
                    crate::require_some(crate::require_ok(decode_v2(
                        GuestPhysicalAddress::new(address),
                        width,
                    )))
                    .register()
                } else {
                    register(address, width)
                };
                match register {
                    DecodedRegister::Model(value) => value,
                    other => panic!("missing SPI {id} register: {other:?}"),
                }
            };
            let cpu = VirtualCpuId::new(0);
            let mut controller = controller();
            let priority = decode(0x400 + u64::from(id), AccessWidth::Byte);
            crate::require_ok(write_model_register(&mut controller, cpu, priority, 0xa1));
            assert_eq!(
                crate::require_ok(read_model_register(&controller, cpu, priority)),
                if v2 { 0xa0 } else { 0xa1 },
            );
            let configuration = decode(0xc00 + u64::from(id / 16 * 4), AccessWidth::Word);
            crate::require_ok(write_model_register(
                &mut controller,
                cpu,
                configuration,
                2 << ((id % 16) * 2),
            ));
            assert_eq!(
                crate::require_ok(controller.snapshot(interrupt(id), cpu)).trigger,
                InterruptTrigger::Edge,
            );
            let route = if v2 {
                decode(0x800 + u64::from(id), AccessWidth::Byte)
            } else {
                decode(0x6000 + u64::from(id) * 8, AccessWidth::DoubleWord)
            };
            crate::require_ok(write_model_register(
                &mut controller,
                cpu,
                route,
                u64::from(v2),
            ));
            let enable = decode(0x100 + u64::from(id / 32 * 4), AccessWidth::Word);
            crate::require_ok(write_model_register(
                &mut controller,
                cpu,
                enable,
                1 << (id % 32),
            ));
            crate::require_ok(controller.inject(interrupt(id), cpu));
            let mut slots = [None];
            crate::require_ok(controller.refill(cpu, &mut slots));
            assert_eq!(crate::require_some(slots[0]).interrupt.get(), id);
        }
        for offset in [0x120, 0x500, 0xc40] {
            let address = u64::from(DISTRIBUTOR_BASE) + offset;
            let decoded = if v2 {
                crate::require_some(crate::require_ok(decode_v2(
                    GuestPhysicalAddress::new(address),
                    AccessWidth::Word,
                )))
                .register()
            } else {
                register(address, AccessWidth::Word)
            };
            assert_eq!(decoded, DecodedRegister::Reserved);
        }
    }
}

fn sparse_controller(ids: &[u32]) -> VirtualGic {
    let mut controller = crate::require_ok(VirtualGicBuilder::new(1));
    for &id in ids {
        crate::require_ok(controller.configure(
            interrupt(id),
            VirtualCpuId::new(0),
            0x80,
            InterruptGroup::Group1,
            InterruptTrigger::Level,
        ));
    }
    crate::require_ok(controller.finish(1))
}

#[test]
fn separates_gic_frames_and_rejects_complete_span_crossings() {
    let distributor = u64::from(DISTRIBUTOR_BASE);
    let redistributor = u64::from(REDISTRIBUTOR_BASE);
    let sgi = redistributor + 0x1_0000;

    let decoded = crate::require_some(crate::require_ok(decode_v3(
        GuestPhysicalAddress::new(distributor),
        AccessWidth::Word,
    )));
    assert_eq!(decoded.frame(), Frame::Distributor);
    let decoded = crate::require_some(crate::require_ok(decode_v3(
        GuestPhysicalAddress::new(redistributor),
        AccessWidth::Word,
    )));
    assert_eq!(decoded.frame(), Frame::RedistributorControl);
    let decoded = crate::require_some(crate::require_ok(decode_v3(
        GuestPhysicalAddress::new(sgi),
        AccessWidth::Byte,
    )));
    assert_eq!(decoded.frame(), Frame::RedistributorSgi);

    assert_eq!(
        decode(
            distributor + u64::from(DISTRIBUTOR_SIZE) - 1,
            AccessWidth::HalfWord
        ),
        Err(DecodeError::CrossesFrame)
    );
    assert_eq!(
        decode(sgi - 1, AccessWidth::HalfWord),
        Err(DecodeError::CrossesFrame)
    );
    assert_eq!(
        decode(
            redistributor + u64::from(REDISTRIBUTOR_SIZE) - 1,
            AccessWidth::DoubleWord,
        ),
        Err(DecodeError::CrossesFrame)
    );
}

#[test]
fn leaves_addresses_outside_gic_frames_unclaimed() {
    let distributor_end = u64::from(DISTRIBUTOR_BASE) + u64::from(DISTRIBUTOR_SIZE);
    let redistributor = u64::from(REDISTRIBUTOR_BASE);
    let redistributor_end = redistributor + u64::from(REDISTRIBUTOR_SIZE);

    assert_eq!(decode(distributor_end, AccessWidth::Word), Ok(None));
    assert_eq!(decode(redistributor - 1, AccessWidth::Byte), Ok(None));
    assert_eq!(decode(redistributor_end, AccessWidth::Byte), Ok(None));
    assert_eq!(decode(u64::MAX, AccessWidth::DoubleWord), Ok(None));
}

#[test]
fn decodes_only_exact_bitmap_and_configuration_words() {
    let distributor = u64::from(DISTRIBUTOR_BASE);
    assert_eq!(
        model_register(distributor + 0x0104, AccessWidth::Word).descriptor(),
        ModelRegisterDescriptor::Bitmap {
            register: BitmapRegister::SetEnable,
            first_interrupt: 32,
        }
    );
    for offset in [0x0101, 0x0102, 0x0103, 0x0105, 0x0106, 0x0107] {
        assert_eq!(
            decode(distributor + offset, AccessWidth::Word),
            Err(DecodeError::InvalidRegisterAccess)
        );
    }
    assert_eq!(
        model_register(distributor + 0x0c08, AccessWidth::Word).descriptor(),
        ModelRegisterDescriptor::Configuration {
            first_interrupt: 32,
        }
    );
    assert_eq!(
        model_register(distributor + 0x0c0c, AccessWidth::Word).descriptor(),
        ModelRegisterDescriptor::Configuration {
            first_interrupt: 48,
        }
    );
    assert_eq!(
        decode(distributor + 0x0c09, AccessWidth::Word),
        Err(DecodeError::InvalidRegisterAccess)
    );
}

#[test]
fn fixed_32_bit_registers_require_one_exact_word() {
    let distributor = u64::from(DISTRIBUTOR_BASE);
    assert_eq!(
        register(distributor, AccessWidth::Word),
        DecodedRegister::Service(ServiceRegister::DistributorControl)
    );
    for (offset, width) in [
        (0x0000, AccessWidth::Byte),
        (0x0000, AccessWidth::HalfWord),
        (0x0000, AccessWidth::DoubleWord),
        (0x0001, AccessWidth::Word),
    ] {
        assert_eq!(
            decode(distributor + offset, width),
            Err(DecodeError::InvalidRegisterAccess)
        );
    }
}

#[test]
fn decodes_type2_and_status_at_their_architectural_offsets() {
    let distributor = u64::from(DISTRIBUTOR_BASE);
    let redistributor = u64::from(REDISTRIBUTOR_BASE);
    assert_eq!(
        register(distributor + 0x000c, AccessWidth::Word),
        DecodedRegister::Service(ServiceRegister::DistributorType2)
    );
    assert_eq!(
        register(distributor + 0x0010, AccessWidth::Word),
        DecodedRegister::Service(ServiceRegister::DistributorStatus)
    );
    assert_eq!(
        register(redistributor + 0x0010, AccessWidth::Word),
        DecodedRegister::Service(ServiceRegister::RedistributorStatus)
    );
}

#[test]
fn service_state_models_status_as_res0_and_masks_distributor_control() {
    let mut state = RegisterState::new();
    assert_eq!(state.read(ServiceRegister::DistributorType2), 0);
    assert_eq!(state.read(ServiceRegister::DistributorStatus), 0);
    assert_eq!(state.read(ServiceRegister::RedistributorStatus), 0);

    state.write(ServiceRegister::DistributorControl, u64::MAX);
    assert_eq!(
        state.read(ServiceRegister::DistributorControl),
        (1 << 4) | (1 << 1) | 1
    );
    state.write(ServiceRegister::DistributorStatus, u64::MAX);
    assert_eq!(state.read(ServiceRegister::DistributorStatus), 0);
}

#[test]
fn priority_registers_accept_bytes_and_aligned_words_only() {
    let distributor = u64::from(DISTRIBUTOR_BASE);
    assert_eq!(
        model_register(distributor + 0x0420, AccessWidth::Byte).descriptor(),
        ModelRegisterDescriptor::Priority {
            first_interrupt: 32,
            count: 1,
        }
    );
    assert_eq!(
        model_register(distributor + 0x0424, AccessWidth::Word).descriptor(),
        ModelRegisterDescriptor::Priority {
            first_interrupt: 36,
            count: 4,
        }
    );
    assert_eq!(
        model_register(distributor + 0x043f, AccessWidth::Byte).descriptor(),
        ModelRegisterDescriptor::Priority {
            first_interrupt: 63,
            count: 1,
        }
    );
    for (offset, width) in [
        (0x0420, AccessWidth::HalfWord),
        (0x0420, AccessWidth::DoubleWord),
        (0x0421, AccessWidth::Word),
        (0x043d, AccessWidth::Word),
    ] {
        assert_eq!(
            decode(distributor + offset, width),
            Err(DecodeError::InvalidRegisterAccess)
        );
    }
    assert_eq!(
        register(distributor + 0x0500, AccessWidth::Word),
        DecodedRegister::Reserved
    );
}

#[test]
fn route_decoder_maps_exact_modeled_spi_registers() {
    let distributor = u64::from(DISTRIBUTOR_BASE);
    for (offset, interrupt) in [
        (0x6100, 32),
        (0x6108, 33),
        (0x61f8, 63),
        (0x6200, 64),
        (0x63f8, 127),
        (0x6400, 128),
        (0x67f8, 255),
    ] {
        let ModelRegisterDescriptor::Route(route) =
            model_register(distributor + offset, AccessWidth::DoubleWord).descriptor()
        else {
            panic!("expected a route register");
        };
        assert_eq!(route.interrupt(), interrupt);
    }
    for (offset, width) in [
        (0x6101, AccessWidth::DoubleWord),
        (0x6104, AccessWidth::Word),
        (0x61fc, AccessWidth::DoubleWord),
    ] {
        assert_eq!(
            decode(distributor + offset, width),
            Err(DecodeError::InvalidRegisterAccess)
        );
    }
    assert_eq!(
        register(
            u64::from(REDISTRIBUTOR_BASE) + 0x1_0000 + 0x6100,
            AccessWidth::DoubleWord,
        ),
        DecodedRegister::Reserved
    );
}

#[test]
fn nonexistent_affinity_route_reads_back_and_disables_delivery() {
    let distributor = u64::from(DISTRIBUTOR_BASE);
    let route = model_register(distributor + 0x6100, AccessWidth::DoubleWord);
    let mut controller = controller();

    crate::require_ok(write_model_register(
        &mut controller,
        VirtualCpuId::new(0),
        route,
        0,
    ));
    assert_eq!(
        crate::require_ok(read_model_register(
            &controller,
            VirtualCpuId::new(0),
            route,
        )),
        0
    );
    crate::require_ok(write_model_register(
        &mut controller,
        VirtualCpuId::new(0),
        route,
        1,
    ));
    assert_eq!(
        crate::require_ok(read_model_register(
            &controller,
            VirtualCpuId::new(0),
            route
        )),
        1
    );
    assert!(!crate::require_ok(controller.snapshot(interrupt(32), VirtualCpuId::new(0))).routed);
    assert_eq!(
        crate::require_ok(controller.snapshot(interrupt(32), VirtualCpuId::new(0)),).target,
        VirtualCpuId::new(0)
    );
}

#[test]
fn redistributor_type_requires_one_complete_doubleword() {
    let redistributor = u64::from(REDISTRIBUTOR_BASE);
    assert_eq!(
        register(redistributor + 0x0008, AccessWidth::DoubleWord),
        DecodedRegister::Service(ServiceRegister::RedistributorType)
    );
    for (offset, width) in [
        (0x0008, AccessWidth::Word),
        (0x000c, AccessWidth::Word),
        (0x0009, AccessWidth::DoubleWord),
    ] {
        assert_eq!(
            decode(redistributor + offset, width),
            Err(DecodeError::InvalidRegisterAccess)
        );
    }
}

#[test]
fn exact_active_registers_decode_and_expose_model_state() {
    let distributor = u64::from(DISTRIBUTOR_BASE);
    assert_eq!(
        model_register(distributor + 0x0304, AccessWidth::Word).descriptor(),
        ModelRegisterDescriptor::Bitmap {
            register: BitmapRegister::SetActive,
            first_interrupt: 32,
        }
    );
    assert_eq!(
        model_register(distributor + 0x0384, AccessWidth::Word).descriptor(),
        ModelRegisterDescriptor::Bitmap {
            register: BitmapRegister::ClearActive,
            first_interrupt: 32,
        }
    );

    let mut controller = controller();
    let access = model_register(distributor + 0x0304, AccessWidth::Word);
    assert_eq!(
        crate::require_ok(read_model_register(
            &controller,
            VirtualCpuId::new(0),
            access,
        )),
        0
    );
    crate::require_ok(write_model_register(
        &mut controller,
        VirtualCpuId::new(0),
        access,
        u32::MAX.into(),
    ));
    assert_eq!(
        crate::require_ok(read_model_register(
            &controller,
            VirtualCpuId::new(0),
            access
        )),
        u64::from(u32::MAX)
    );
}

#[test]
fn group_words_roundtrip_zero_and_one_bits() {
    let distributor = u64::from(DISTRIBUTOR_BASE);
    let access = model_register(distributor + 0x0084, AccessWidth::Word);
    let mut controller = controller();
    let groups = 0xa55a_0ff0u64;

    crate::require_ok(write_model_register(
        &mut controller,
        VirtualCpuId::new(0),
        access,
        groups,
    ));
    assert_eq!(
        crate::require_ok(read_model_register(
            &controller,
            VirtualCpuId::new(0),
            access,
        )),
        groups
    );
    for bit in 0..32 {
        let snapshot =
            crate::require_ok(controller.snapshot(interrupt(32 + bit), VirtualCpuId::new(0)));
        assert_eq!(
            snapshot.group,
            if groups & (1 << bit) == 0 {
                InterruptGroup::Group0
            } else {
                InterruptGroup::Group1
            }
        );
    }
}

#[test]
fn priority_accesses_update_only_their_decoded_lanes() {
    let distributor = u64::from(DISTRIBUTOR_BASE);
    let word = model_register(distributor + 0x0424, AccessWidth::Word);
    let byte = model_register(distributor + 0x0427, AccessWidth::Byte);
    let mut controller = controller();

    crate::require_ok(write_model_register(
        &mut controller,
        VirtualCpuId::new(0),
        word,
        0x4433_2211,
    ));
    crate::require_ok(write_model_register(
        &mut controller,
        VirtualCpuId::new(0),
        byte,
        0xaa,
    ));
    assert_eq!(
        crate::require_ok(read_model_register(&controller, VirtualCpuId::new(0), word,)),
        0xaa33_2211
    );
    assert_eq!(
        crate::require_ok(controller.snapshot(interrupt(35), VirtualCpuId::new(0)),).priority,
        0x80
    );
    assert_eq!(
        crate::require_ok(controller.snapshot(interrupt(40), VirtualCpuId::new(0)),).priority,
        0x80
    );
}

#[test]
fn malformed_accesses_are_rejected_before_model_mutation() {
    let distributor = u64::from(DISTRIBUTOR_BASE);
    let controller = controller();
    let before = crate::require_ok(controller.snapshot(interrupt(32), VirtualCpuId::new(0)));

    for (offset, width) in [
        (0x0085, AccessWidth::Word),
        (0x0421, AccessWidth::Word),
        (0x0c09, AccessWidth::Word),
        (0x6101, AccessWidth::DoubleWord),
    ] {
        let decoded = decode_v3(GuestPhysicalAddress::new(distributor + offset), width);
        assert_eq!(decoded, Err(DecodeError::InvalidRegisterAccess));
        // No DecodedAccess exists, so the production model mutation API cannot
        // be invoked for this malformed transaction.
        assert_eq!(
            crate::require_ok(controller.snapshot(interrupt(32), VirtualCpuId::new(0)),),
            before
        );
    }
}

#[test]
fn sparse_model_writes_fail_before_mutating_an_earlier_lane() {
    let distributor = u64::from(DISTRIBUTOR_BASE);

    let group = model_register(distributor + 0x0084, AccessWidth::Word);
    let mut sparse = sparse_controller(&[32]);
    let before = crate::require_ok(sparse.snapshot(interrupt(32), VirtualCpuId::new(0)));
    assert!(write_model_register(&mut sparse, VirtualCpuId::new(0), group, 0).is_err());
    assert_eq!(
        crate::require_ok(sparse.snapshot(interrupt(32), VirtualCpuId::new(0))),
        before
    );

    let enable = model_register(distributor + 0x0104, AccessWidth::Word);
    let mut sparse = sparse_controller(&[32]);
    assert!(write_model_register(&mut sparse, VirtualCpuId::new(0), enable, 0b101).is_err());
    assert!(!crate::require_ok(sparse.snapshot(interrupt(32), VirtualCpuId::new(0))).enabled);

    let priority = model_register(distributor + 0x0420, AccessWidth::Word);
    let mut sparse = sparse_controller(&[32, 33]);
    assert!(
        write_model_register(&mut sparse, VirtualCpuId::new(0), priority, 0x4433_2211,).is_err()
    );
    assert_eq!(
        crate::require_ok(sparse.snapshot(interrupt(32), VirtualCpuId::new(0))).priority,
        0x80
    );

    let configuration = model_register(distributor + 0x0c08, AccessWidth::Word);
    let mut sparse = sparse_controller(&[32, 33]);
    assert!(
        write_model_register(
            &mut sparse,
            VirtualCpuId::new(0),
            configuration,
            u32::MAX.into(),
        )
        .is_err()
    );
    assert_eq!(
        crate::require_ok(sparse.snapshot(interrupt(32), VirtualCpuId::new(0))).trigger,
        InterruptTrigger::Level
    );
}

#[test]
fn gicv2_decodes_private_interrupts_and_rejects_cross_frame_accesses() {
    use hyper::vm::arm::gic::mmio::decode_v2;
    let decoded = crate::require_some(crate::require_ok(decode_v2(
        GuestPhysicalAddress::new(u64::from(DISTRIBUTOR_BASE) + 0x100),
        AccessWidth::Word,
    )));
    let DecodedRegister::Model(model) = decoded.register() else {
        panic!("not a model register")
    };
    assert_eq!(
        model.descriptor(),
        ModelRegisterDescriptor::Bitmap {
            register: BitmapRegister::SetEnable,
            first_interrupt: 0
        }
    );
    assert_eq!(
        decode_v2(
            GuestPhysicalAddress::new(u64::from(DISTRIBUTOR_BASE) + 0xfff),
            AccessWidth::Word
        ),
        Err(DecodeError::CrossesFrame)
    );
    assert!(
        crate::require_ok(decode_v2(
            GuestPhysicalAddress::new(0x0801_0000),
            AccessWidth::Word
        ))
        .is_none()
    );
    let state = RegisterState::new();
    assert_eq!(
        state.read(ServiceRegister::DistributorTypeV2),
        u64::from(INTERRUPT_COUNT / 32 - 1)
    );
    assert_eq!(state.read(ServiceRegister::PeripheralId2V2), 0x20);
}

#[test]
fn gicv2_target_mask_and_priority_precision_preserve_pending() {
    use hyper::vm::arm::gic::mmio::decode_v2;
    let cpu = VirtualCpuId::new(0);
    let mut controller = controller();
    let decode = |offset| {
        let access = crate::require_some(crate::require_ok(decode_v2(
            GuestPhysicalAddress::new(u64::from(DISTRIBUTOR_BASE) + offset),
            AccessWidth::Byte,
        )));
        match access.register() {
            DecodedRegister::Model(register) => register,
            other => panic!("not a model register: {other:?}"),
        }
    };
    crate::require_ok(write_model_register(
        &mut controller,
        cpu,
        decode(0x420),
        0xaf,
    ));
    assert_eq!(
        crate::require_ok(read_model_register(&controller, cpu, decode(0x420))),
        0xa8
    );
    crate::require_ok(controller.set_enabled(interrupt(32), cpu, true));
    crate::require_ok(controller.inject(interrupt(32), cpu));
    crate::require_ok(write_model_register(&mut controller, cpu, decode(0x820), 0));
    assert_eq!(
        crate::require_ok(read_model_register(&controller, cpu, decode(0x820))),
        0
    );
    let mut slots = [None];
    crate::require_ok(controller.refill(cpu, &mut slots));
    assert!(slots[0].is_none());
    assert!(crate::require_ok(controller.snapshot(interrupt(32), cpu)).pending);
    crate::require_ok(write_model_register(&mut controller, cpu, decode(0x820), 1));
    crate::require_ok(controller.refill(cpu, &mut slots));
    assert!(slots[0].is_some());
}

#[test]
fn gicv3_banks_are_addressed_by_redistributor_not_accessing_cpu() {
    use hyper::vm::arm::gic::mmio::decode_v3_cpus;
    let state = RegisterState::new();
    for cpu in 0..8u32 {
        let base = u64::from(REDISTRIBUTOR_BASE) + u64::from(cpu) * 0x20000;
        let decoded = crate::require_some(crate::require_ok(decode_v3_cpus(
            GuestPhysicalAddress::new(base + 8),
            AccessWidth::DoubleWord,
            8,
        )));
        assert_eq!(decoded.redistributor(), Some(cpu));
        assert_eq!(
            decoded.register(),
            DecodedRegister::Service(ServiceRegister::RedistributorType)
        );
        let value = state.read_for_cpu(ServiceRegister::RedistributorType, cpu, 8);
        assert_eq!(value >> 32, u64::from(cpu));
        assert_eq!(value & (1 << 4) != 0, cpu == 7);
        let private = crate::require_some(crate::require_ok(decode_v3_cpus(
            GuestPhysicalAddress::new(base + 0x10100),
            AccessWidth::Word,
            8,
        )));
        assert_eq!(private.redistributor(), Some(cpu));
        assert_eq!(private.frame(), Frame::RedistributorSgi);
    }
    assert!(
        crate::require_ok(decode_v3_cpus(
            GuestPhysicalAddress::new(u64::from(REDISTRIBUTOR_BASE) + 8 * 0x20000),
            AccessWidth::Word,
            8,
        ))
        .is_none()
    );
}

#[test]
fn gicv2_sgi_filter_targets_and_banked_source_registers() {
    use hyper::vm::arm::gic::mmio::decode_v2;
    let mut builder = crate::require_ok(VirtualGicBuilder::new(4));
    for cpu in 0..4 {
        for id in 0..16 {
            crate::require_ok(builder.configure(
                interrupt(id),
                VirtualCpuId::new(cpu),
                0x80,
                InterruptGroup::Group0,
                InterruptTrigger::Edge,
            ));
        }
    }
    let mut gic = crate::require_ok(builder.finish(1));
    let decode = |offset| match crate::require_some(crate::require_ok(decode_v2(
        GuestPhysicalAddress::new(u64::from(DISTRIBUTOR_BASE) + offset),
        AccessWidth::Word,
    )))
    .register()
    {
        DecodedRegister::Model(register) => register,
        other => panic!("unexpected {other:?}"),
    };
    let sgi = decode(0xf00);
    crate::require_ok(write_model_register(
        &mut gic,
        VirtualCpuId::new(2),
        sgi,
        3 | (1 << 24),
    ));
    for cpu in 0..4 {
        assert_eq!(
            crate::require_ok(gic.snapshot(interrupt(3), VirtualCpuId::new(cpu))).pending,
            cpu != 2
        );
        assert_eq!(
            crate::require_ok(gic.sgi_sources(interrupt(3), VirtualCpuId::new(cpu))),
            if cpu != 2 { 4 } else { 0 }
        );
    }
    // SGI3 is byte 3 in CPENDSGIR0; writes clear only the indicated source.
    let sources = decode(0xf10);
    assert_eq!(
        crate::require_ok(read_model_register(&gic, VirtualCpuId::new(1), sources)),
        4 << 24
    );
    crate::require_ok(write_model_register(
        &mut gic,
        VirtualCpuId::new(1),
        sources,
        4 << 24,
    ));
    assert!(!crate::require_ok(gic.snapshot(interrupt(3), VirtualCpuId::new(1))).pending);
    assert!(crate::require_ok(gic.snapshot(interrupt(3), VirtualCpuId::new(0))).pending);
    assert_eq!(
        RegisterState::new().read_for_cpu(ServiceRegister::DistributorTypeV2, 0, 4),
        u64::from(INTERRUPT_COUNT / 32 - 1) | (3 << 5)
    );
}

#[test]
fn msi_event_remains_pending_without_a_durable_device_level() {
    let cpu = VirtualCpuId::new(0);
    for trigger in [InterruptTrigger::Level, InterruptTrigger::Edge] {
        let mut builder = crate::require_ok(VirtualGicBuilder::new(1));
        crate::require_ok(builder.configure(
            interrupt(128),
            cpu,
            0x80,
            InterruptGroup::Group1,
            trigger,
        ));
        let mut controller = crate::require_ok(builder.finish(1));
        // An MSI is a pending event, even before Linux has programmed ICFGR.
        // A synthesized assert/deassert level pair would erase the first case.
        crate::require_ok(controller.inject(interrupt(128), cpu));
        assert!(crate::require_ok(controller.snapshot(interrupt(128), cpu)).pending);
        crate::require_ok(controller.set_enabled(interrupt(128), cpu, true));
        let mut slots = [None];
        crate::require_ok(controller.refill(cpu, &mut slots));
        assert_eq!(crate::require_some(slots[0]).interrupt.get(), 128);
    }
}
