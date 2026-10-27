// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Exercises rollback of unpublished VM and scheduler reservations.

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Error {
    Device(crate::vm::device::Error),
    Interrupts(crate::hal::vm::InterruptError),
    Memory(crate::vm::memory::Error),
    Registry(crate::vm::registry::Error),
    DormantVcpuQuiesce(crate::vm::registry::DormantVcpuQuiesceError),
    Scheduler(crate::task::scheduler::Error),
    VcpuPreparation(crate::vm::registry::VcpuPreparationError),
    Resource(crate::accounting::ResourceError),
    Sleep(crate::task::SleepError),
    Accounting,
    ObjectNotFound,
    ProgressTimeout,
    SchedulerThreadLeaked,
    UnsupportedAdmission,
    #[cfg(CONFIG_ARCH_AARCH64)]
    Notification(&'static str),
    #[cfg(CONFIG_ARCH_AARCH64)]
    Quiescence(super::support::QuiescenceError),
}

impl From<crate::task::SleepError> for Error {
    fn from(error: crate::task::SleepError) -> Self {
        Self::Sleep(error)
    }
}

pub(super) fn run() -> Result<(), Error> {
    crate::vm::registry::verify_reservation_rollback().map_err(Error::Registry)?;
    if !crate::task::thread::verify_retirement_charge_lifetime() {
        return Err(Error::Accounting);
    }
    if !crate::hal::vm::guest_execution_available() {
        return verify_unavailable_backend_admission();
    }

    let before = crate::task::scheduler::statistics().map_err(Error::Scheduler)?;
    let (prepared, domain) = prepare_test_vm()?;
    if domain
        .usage()
        .committed(crate::accounting::ResourceKind::VirtualMachines)
        != 1
        || domain
            .usage()
            .committed(crate::accounting::ResourceKind::VirtualCpus)
            != 1
        || domain
            .usage()
            .committed(crate::accounting::ResourceKind::Threads)
            != 1
        || domain
            .usage()
            .committed(crate::accounting::ResourceKind::Timers)
            != 1 + crate::vm::device::timer_count()
        || domain
            .usage()
            .committed(crate::accounting::ResourceKind::KernelObjects)
            != 2
    {
        return Err(Error::Accounting);
    }
    drop(prepared);
    wait_for_vm_usage_release(&domain)?;
    let after = crate::task::scheduler::statistics().map_err(Error::Scheduler)?;
    if after.threads != before.threads {
        return Err(Error::SchedulerThreadLeaked);
    }
    verify_timer_resource_admission()?;
    verify_vcpu_resource_admission()?;
    verify_interrupt_resource_admission()?;

    if crate::hal::vm::try_administrative_stop().is_ok() {
        verify_dormant_vcpu_stop()?;
        verify_thread_object_charge_lifetime()?;
        verify_observed_vm_retirement()?;
        #[cfg(CONFIG_ARCH_AARCH64)]
        verify_notification_delivery()?;
    }
    Ok(())
}

#[cfg(CONFIG_ARCH_AARCH64)]
fn verify_notification_delivery() -> Result<(), Error> {
    let (front, front_domain) = prepare_test_vm()?;
    let (back, back_domain) = prepare_test_vm()?;
    let result = crate::vm::io::Notification::verify_delivery_for_test(front, back, &front_domain)
        .map_err(Error::Notification);
    // Cleanup must run even after a failed assertion in the interleaving test.
    let front_released = wait_for_vm_usage_release(&front_domain);
    let back_released = wait_for_vm_usage_release(&back_domain);
    let quiescent = super::support::quiesce_workers().map_err(Error::Quiescence);
    result?;
    front_released?;
    back_released?;
    quiescent?;
    crate::pr_info!("HypeR test: notification publication interleavings passed");
    Ok(())
}

fn verify_unavailable_backend_admission() -> Result<(), Error> {
    let domain = crate::accounting::ResourceDomain::try_new_root(
        crate::accounting::ResourceLimits::UNLIMITED,
    )
    .map_err(Error::Resource)?;
    let before = domain.usage();
    let (ram_base, _, platform_profile) = test_platform();
    for _ in 0..2 {
        let pending = crate::vm::objects::PendingVirtualMachine::try_new(
            crate::vm::objects::VirtualMachineConfiguration {
                guest_physical_base: ram_base,
                memory_size: 2 * hyper::mm::PAGE_SIZE,
                vcpu_count: 1,
                architecture: crate::hal::vm::guest_architecture_abi(),
                platform_profile,
            },
            &domain,
        );
        if !matches!(
            pending,
            Err(crate::vm::objects::Error::UnsupportedArchitecture)
        ) {
            return Err(Error::UnsupportedAdmission);
        }
        if domain.usage() != before {
            return Err(Error::Accounting);
        }
    }
    Ok(())
}

fn prepare_test_vm() -> Result<
    (
        crate::vm::registry::PreparedVm,
        crate::accounting::ResourceDomain,
    ),
    Error,
> {
    let domain = crate::accounting::ResourceDomain::try_new_root(
        crate::accounting::ResourceLimits::UNLIMITED,
    )
    .map_err(Error::Resource)?;
    let prepared = prepare_test_vm_in(&domain)?;
    Ok((prepared, domain))
}

fn prepare_test_vm_in(
    domain: &crate::accounting::ResourceDomain,
) -> Result<crate::vm::registry::PreparedVm, Error> {
    prepare_test_vm_with(domain, |_, _| Ok(()))
}

fn prepare_test_vm_with(
    domain: &crate::accounting::ResourceDomain,
    initialize: impl FnOnce(&mut crate::vm::memory::GuestAddressSpace, u64) -> Result<(), Error>,
) -> Result<crate::vm::registry::PreparedVm, Error> {
    let lifecycle = crate::vm::registry::VmLifecycleResources::try_reserve(domain, 1)
        .map_err(Error::Registry)?;
    let mut reservation = crate::vm::registry::reserve().map_err(Error::Registry)?;
    let identifier = reservation.take_hardware_vmid().map_err(Error::Registry)?;
    let (ram_base, timer_interrupt, platform_profile) = test_platform();
    let mut address_space = crate::vm::memory::GuestAddressSpace::new(
        identifier,
        ram_base,
        2 * hyper::mm::PAGE_SIZE,
        domain,
    )
    .map_err(Error::Memory)?;
    initialize(&mut address_space, ram_base)?;
    let interrupt_plan = crate::hal::vm::prepare_interrupt_controller(1, timer_interrupt)
        .map_err(Error::Interrupts)?;
    let interrupt_controller_charge = lifecycle
        .reserve_interrupt_controller(
            crate::hal::vm::prepared_interrupt_controller_allocation_size(&interrupt_plan),
        )
        .map_err(Error::Registry)?;
    let interrupts = crate::hal::vm::create_prepared_interrupt_controller(interrupt_plan)
        .map_err(Error::Interrupts)?;
    let devices = crate::vm::device::prepare(None).map_err(Error::Device)?;
    let builder = crate::vm::registry::VmBuilder::new(
        reservation,
        lifecycle,
        crate::vm::objects::VirtualMachineConfiguration {
            guest_physical_base: ram_base,
            memory_size: 2 * hyper::mm::PAGE_SIZE,
            vcpu_count: 1,
            architecture: crate::hal::vm::guest_architecture_abi(),
            platform_profile,
        },
        address_space,
        interrupts,
        interrupt_controller_charge,
        devices,
    )
    .map_err(Error::Registry)?;
    let prepared = builder
        .prepare_boot_vcpu(
            0,
            crate::vm::objects::VirtualCpuBootstrap {
                entry: ram_base,
                stack: 0,
                arguments: [ram_base + hyper::mm::PAGE_SIZE, 0, 0, 0],
            },
        )
        .map_err(Error::VcpuPreparation)?;
    Ok(prepared)
}

fn verify_timer_resource_admission() -> Result<(), Error> {
    use {crate::accounting::ResourceKind, crate::accounting::ResourceLimits};

    // One endpoint timer plus the selected platform's reserved device timers
    // must be admitted before any backing or scheduler object is allocated.
    let required = 1 + crate::vm::device::timer_count();
    let domain = crate::accounting::ResourceDomain::try_new_root(
        ResourceLimits::UNLIMITED.with(ResourceKind::Timers, required - 1),
    )
    .map_err(Error::Resource)?;
    if !matches!(
        prepare_test_vm_in(&domain),
        Err(Error::Registry(crate::vm::registry::Error::Resource(
            crate::accounting::ResourceError::LimitExceeded {
                resource: ResourceKind::Timers,
                ..
            }
        )))
    ) || !vm_usage_released(&domain)
    {
        return Err(Error::Accounting);
    }
    Ok(())
}

fn verify_vcpu_resource_admission() -> Result<(), Error> {
    use {crate::accounting::ResourceKind, crate::accounting::ResourceLimits};

    let domain = crate::accounting::ResourceDomain::try_new_root(
        ResourceLimits::UNLIMITED.with(ResourceKind::Threads, 0),
    )
    .map_err(Error::Resource)?;
    if !matches!(
        prepare_test_vm_in(&domain),
        Err(Error::VcpuPreparation(
            crate::vm::registry::VcpuPreparationError::Registry(
                crate::vm::registry::Error::Resource(
                    crate::accounting::ResourceError::LimitExceeded {
                        resource: ResourceKind::Threads,
                        ..
                    }
                )
            )
        ))
    ) || !vm_usage_released(&domain)
    {
        return Err(Error::Accounting);
    }
    Ok(())
}

fn verify_interrupt_resource_admission() -> Result<(), Error> {
    use {crate::accounting::ResourceKind, crate::accounting::ResourceLimits};

    let domain = crate::accounting::ResourceDomain::try_new_root(ResourceLimits::UNLIMITED)
        .map_err(Error::Resource)?;
    let lifecycle = crate::vm::registry::VmLifecycleResources::try_reserve(&domain, 1)
        .map_err(Error::Registry)?;
    let (_, timer_interrupt, _) = test_platform();
    let plan = crate::hal::vm::prepare_interrupt_controller(1, timer_interrupt)
        .map_err(Error::Interrupts)?;
    let allocation = crate::hal::vm::prepared_interrupt_controller_allocation_size(&plan);
    if allocation == 0 {
        drop(lifecycle);
        return if vm_usage_released(&domain) {
            Ok(())
        } else {
            Err(Error::Accounting)
        };
    }

    let used = domain.usage().total(ResourceKind::KernelMemoryBytes);
    domain
        .set_local_limits(ResourceLimits::UNLIMITED.with(ResourceKind::KernelMemoryBytes, used))
        .map_err(Error::Resource)?;
    if !matches!(
        lifecycle.reserve_interrupt_controller(allocation),
        Err(crate::vm::registry::Error::Resource(
            crate::accounting::ResourceError::LimitExceeded {
                resource: ResourceKind::KernelMemoryBytes,
                ..
            }
        ))
    ) || domain.usage().total(ResourceKind::KernelMemoryBytes) != used
    {
        return Err(Error::Accounting);
    }
    drop(lifecycle);
    if !vm_usage_released(&domain) {
        return Err(Error::Accounting);
    }
    Ok(())
}

fn verify_dormant_vcpu_stop() -> Result<(), Error> {
    let (prepared, domain) = prepare_test_vm()?;
    let installed = prepared.install().map_err(Error::Registry)?;
    crate::vm::registry::verify_dormant_vcpu_quiesce(installed)
        .map_err(Error::DormantVcpuQuiesce)?;
    wait_for_vm_usage_release(&domain)?;
    Ok(())
}

fn verify_thread_object_charge_lifetime() -> Result<(), Error> {
    use crate::accounting::ResourceKind;

    let (prepared, domain) = prepare_test_vm()?;
    let installed = prepared.install().map_err(Error::Registry)?;
    let thread = installed.boot_vcpu_for_test();
    let object = crate::task::scheduler::thread_object_snapshot(thread)
        .map_err(Error::Scheduler)?
        .object;
    let diagnostic = crate::object::retain_for_test(object.koid).ok_or(Error::ObjectNotFound)?;

    crate::vm::registry::verify_dormant_vcpu_quiesce(installed)
        .map_err(Error::DormantVcpuQuiesce)?;
    let object_bytes = crate::task::system_thread_object_allocation_size()
        .and_then(|bytes| u64::try_from(bytes).ok())
        .ok_or(Error::Accounting)?;
    if domain.usage().total(ResourceKind::KernelObjects) != 1
        || domain.usage().total(ResourceKind::KernelMemoryBytes) != object_bytes
        || [
            ResourceKind::CommittedPages,
            ResourceKind::Threads,
            ResourceKind::Timers,
            ResourceKind::VirtualMachines,
            ResourceKind::VirtualCpus,
        ]
        .into_iter()
        .any(|kind| domain.usage().total(kind) != 0)
    {
        return Err(Error::Accounting);
    }

    drop(diagnostic);
    if !crate::task::wait_for_test_progress(crate::task::TEST_PROGRESS_TIMEOUT_NS, || {
        Ok::<_, Error>(vm_usage_released(&domain))
    })? {
        return Err(Error::ProgressTimeout);
    }
    Ok(())
}

pub(super) fn wait_for_vm_usage_release(
    domain: &crate::accounting::ResourceDomain,
) -> Result<(), Error> {
    if !crate::task::wait_for_test_progress(crate::task::TEST_PROGRESS_TIMEOUT_NS, || {
        Ok::<_, Error>(vm_usage_released(domain))
    })? {
        return Err(Error::ProgressTimeout);
    }
    Ok(())
}

fn vm_usage_released(domain: &crate::accounting::ResourceDomain) -> bool {
    use crate::accounting::ResourceKind;

    [
        ResourceKind::KernelMemoryBytes,
        ResourceKind::KernelObjects,
        ResourceKind::CommittedPages,
        ResourceKind::Threads,
        ResourceKind::Timers,
        ResourceKind::VirtualMachines,
        ResourceKind::VirtualCpus,
    ]
    .into_iter()
    .all(|kind| domain.usage().total(kind) == 0)
}

// Dormant lifecycle fixtures never execute a payload and require no Linux ABI.
fn test_platform() -> (u64, hyper_vm_models::interrupt::VirtualInterruptId, u32) {
    #[cfg(CONFIG_ARCH_RISCV64)]
    let (ram_base, profile) = (
        hyper::abi::native::HYPER_NATIVE_VIRTUAL_PLATFORM_RISCV64_REFERENCE_GUEST_RAM_BASE,
        hyper::abi::native::HYPER_NATIVE_VIRTUAL_PLATFORM_RISCV64_REFERENCE,
    );
    #[cfg(not(CONFIG_ARCH_RISCV64))]
    let (ram_base, profile) = (
        hyper::abi::native::HYPER_NATIVE_VIRTUAL_PLATFORM_AARCH64_REFERENCE_GUEST_RAM_BASE,
        hyper::abi::native::HYPER_NATIVE_VIRTUAL_PLATFORM_AARCH64_REFERENCE,
    );
    (
        ram_base,
        crate::vm::device::default_timer_interrupt(),
        profile as u32,
    )
}

fn verify_observed_vm_retirement() -> Result<(), Error> {
    use crate::accounting::ResourceKind;

    for _ in 0..4 {
        let (prepared, domain) = prepare_test_vm()?;
        let installed = prepared.install().map_err(Error::Registry)?;
        let observer = crate::vm::registry::verify_observed_vm_quiesce(installed)
            .map_err(Error::DormantVcpuQuiesce)?;
        // The observer owns endpoint/tombstone metadata, but no guest RAM or
        // scheduler stacks. Their charges must drain before identity reuse.
        if [
            ResourceKind::Threads,
            ResourceKind::CommittedPages,
            ResourceKind::GuestPages,
        ]
        .into_iter()
        .any(|kind| domain.usage().total(kind) != 0)
        {
            return Err(Error::Accounting);
        }
        drop(observer);
        wait_for_vm_usage_release(&domain)?;
    }
    crate::pr_info!(
        "HypeR test: retained VM observers permit retirement and slot reuse (4 cycles)"
    );
    Ok(())
}

/// Shares the dormant lifecycle fixture while installing a real executable guest.
#[cfg(any(CONFIG_ARCH_AARCH64, CONFIG_ARCH_RISCV64))]
pub(super) fn prepare_migration_guest(
    code: &[u8],
) -> Result<
    (
        crate::vm::registry::PreparedVm,
        crate::accounting::ResourceDomain,
        *const core::sync::atomic::AtomicU64,
    ),
    Error,
> {
    let domain = crate::accounting::ResourceDomain::try_new_root(
        crate::accounting::ResourceLimits::UNLIMITED,
    )
    .map_err(Error::Resource)?;
    let mut counter = core::ptr::null();
    let prepared = prepare_test_vm_with(&domain, |memory, base| {
        memory.copy_to(base, code).map_err(Error::Memory)?;
        memory
            .copy_to(base + hyper::mm::PAGE_SIZE, &[0; 8])
            .map_err(Error::Memory)?;
        counter = memory
            .atomic_counter_for_test(base + hyper::mm::PAGE_SIZE)
            .map_err(Error::Memory)?;
        memory
            .publish_resident_instructions()
            .map_err(Error::Memory)?;
        memory.finish_boot_loading();
        Ok(())
    })?;
    Ok((prepared, domain, counter))
}
