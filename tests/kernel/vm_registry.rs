// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Exercises rollback of unpublished VM and scheduler reservations.

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Error {
    Device(crate::kernel::vm::device::Error),
    Interrupts(crate::hal::vm::InterruptError),
    Memory(crate::kernel::vm::memory::Error),
    Registry(crate::kernel::vm::registry::Error),
    DormantVcpuQuiesce(crate::kernel::vm::registry::DormantVcpuQuiesceError),
    Scheduler(crate::kernel::task::scheduler::Error),
    VcpuPreparation(crate::kernel::vm::registry::VcpuPreparationError),
    Resource(crate::kernel::accounting::ResourceError),
    Sleep(crate::kernel::task::SleepError),
    Accounting,
    ObjectNotFound,
    ProgressTimeout,
    InitialContext,
    SchedulerThreadLeaked,
}

impl From<crate::kernel::task::SleepError> for Error {
    fn from(error: crate::kernel::task::SleepError) -> Self {
        Self::Sleep(error)
    }
}

pub(super) fn run() -> Result<(), Error> {
    crate::kernel::vm::registry::verify_reservation_rollback().map_err(Error::Registry)?;
    if !crate::kernel::task::thread::verify_retirement_charge_lifetime() {
        return Err(Error::Accounting);
    }
    if !crate::hal::vm::guest_execution_available() {
        return Ok(());
    }

    let before = crate::kernel::task::scheduler::statistics().map_err(Error::Scheduler)?;
    let (prepared, domain) = prepare_test_vm()?;
    if domain
        .usage()
        .committed(crate::kernel::accounting::ResourceKind::VirtualMachines)
        != 1
        || domain
            .usage()
            .committed(crate::kernel::accounting::ResourceKind::VirtualCpus)
            != 1
        || domain
            .usage()
            .committed(crate::kernel::accounting::ResourceKind::Threads)
            != 1
        || domain
            .usage()
            .committed(crate::kernel::accounting::ResourceKind::Timers)
            != 1 + crate::kernel::vm::device::timer_count()
        || domain
            .usage()
            .committed(crate::kernel::accounting::ResourceKind::KernelObjects)
            != 2
    {
        return Err(Error::Accounting);
    }
    drop(prepared);
    wait_for_vm_usage_release(&domain)?;
    let after = crate::kernel::task::scheduler::statistics().map_err(Error::Scheduler)?;
    if after.threads != before.threads {
        return Err(Error::SchedulerThreadLeaked);
    }
    verify_timer_resource_admission()?;
    verify_vcpu_resource_admission()?;
    verify_interrupt_resource_admission()?;

    if crate::hal::vm::try_administrative_stop().is_ok() {
        verify_dormant_vcpu_stop()?;
        verify_thread_object_charge_lifetime()?;
    }
    Ok(())
}

fn prepare_test_vm() -> Result<
    (
        crate::kernel::vm::registry::PreparedVm,
        crate::kernel::accounting::ResourceDomain,
    ),
    Error,
> {
    let domain = crate::kernel::accounting::ResourceDomain::try_new_root(
        crate::kernel::accounting::ResourceLimits::UNLIMITED,
    )
    .map_err(Error::Resource)?;
    let prepared = prepare_test_vm_in(&domain)?;
    Ok((prepared, domain))
}

fn prepare_test_vm_in(
    domain: &crate::kernel::accounting::ResourceDomain,
) -> Result<crate::kernel::vm::registry::PreparedVm, Error> {
    let lifecycle = crate::kernel::vm::registry::VmLifecycleResources::try_reserve(domain, 1)
        .map_err(Error::Registry)?;
    let mut reservation = crate::kernel::vm::registry::reserve().map_err(Error::Registry)?;
    let identifier = reservation.take_hardware_vmid().map_err(Error::Registry)?;
    let (ram_base, timer_interrupt, platform_profile) = test_platform();
    let address_space = crate::kernel::vm::memory::GuestAddressSpace::new(
        identifier,
        ram_base,
        2 * hyper::mm::PAGE_SIZE,
        domain,
    )
    .map_err(Error::Memory)?;
    let interrupt_plan = crate::hal::vm::prepare_interrupt_controller(1, timer_interrupt)
        .map_err(Error::Interrupts)?;
    let interrupt_controller_charge = lifecycle
        .reserve_interrupt_controller(
            crate::hal::vm::prepared_interrupt_controller_allocation_size(&interrupt_plan),
        )
        .map_err(Error::Registry)?;
    let interrupts = crate::hal::vm::create_prepared_interrupt_controller(interrupt_plan)
        .map_err(Error::Interrupts)?;
    let devices = crate::kernel::vm::device::prepare(None).map_err(Error::Device)?;
    let builder = crate::kernel::vm::registry::VmBuilder::new(
        reservation,
        lifecycle,
        crate::kernel::vm::objects::VirtualMachineConfiguration {
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
            crate::hal::vm::prepare_initial_context(ram_base, &[])
                .map_err(|_| Error::InitialContext)?,
        )
        .map_err(Error::VcpuPreparation)?;
    Ok(prepared)
}

fn verify_timer_resource_admission() -> Result<(), Error> {
    use crate::kernel::accounting::{ResourceKind, ResourceLimits};

    // One endpoint timer plus the selected platform's reserved device timers
    // must be admitted before any backing or scheduler object is allocated.
    let required = 1 + crate::kernel::vm::device::timer_count();
    let domain = crate::kernel::accounting::ResourceDomain::try_new_root(
        ResourceLimits::UNLIMITED.with(ResourceKind::Timers, required - 1),
    )
    .map_err(Error::Resource)?;
    if !matches!(
        prepare_test_vm_in(&domain),
        Err(Error::Registry(
            crate::kernel::vm::registry::Error::Resource(
                crate::kernel::accounting::ResourceError::LimitExceeded {
                    resource: ResourceKind::Timers,
                    ..
                }
            )
        ))
    ) || !vm_usage_released(&domain)
    {
        return Err(Error::Accounting);
    }
    Ok(())
}

fn verify_vcpu_resource_admission() -> Result<(), Error> {
    use crate::kernel::accounting::{ResourceKind, ResourceLimits};

    let domain = crate::kernel::accounting::ResourceDomain::try_new_root(
        ResourceLimits::UNLIMITED.with(ResourceKind::Threads, 0),
    )
    .map_err(Error::Resource)?;
    if !matches!(
        prepare_test_vm_in(&domain),
        Err(Error::VcpuPreparation(
            crate::kernel::vm::registry::VcpuPreparationError::Registry(
                crate::kernel::vm::registry::Error::Resource(
                    crate::kernel::accounting::ResourceError::LimitExceeded {
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
    use crate::kernel::accounting::{ResourceKind, ResourceLimits};

    let domain = crate::kernel::accounting::ResourceDomain::try_new_root(ResourceLimits::UNLIMITED)
        .map_err(Error::Resource)?;
    let lifecycle = crate::kernel::vm::registry::VmLifecycleResources::try_reserve(&domain, 1)
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
        Err(crate::kernel::vm::registry::Error::Resource(
            crate::kernel::accounting::ResourceError::LimitExceeded {
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
    crate::kernel::vm::registry::verify_dormant_vcpu_quiesce(installed)
        .map_err(Error::DormantVcpuQuiesce)?;
    wait_for_vm_usage_release(&domain)?;
    Ok(())
}

fn verify_thread_object_charge_lifetime() -> Result<(), Error> {
    use crate::kernel::accounting::ResourceKind;

    let (prepared, domain) = prepare_test_vm()?;
    let installed = prepared.install().map_err(Error::Registry)?;
    let thread = installed.boot_vcpu_for_test();
    let object = crate::kernel::task::scheduler::thread_object_snapshot(thread)
        .map_err(Error::Scheduler)?
        .object;
    let diagnostic =
        crate::kernel::object::retain_for_test(object.koid).ok_or(Error::ObjectNotFound)?;

    crate::kernel::vm::registry::verify_dormant_vcpu_quiesce(installed)
        .map_err(Error::DormantVcpuQuiesce)?;
    let object_bytes = crate::kernel::task::system_thread_object_allocation_size()
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
    if !crate::kernel::task::wait_for_test_progress(
        crate::kernel::task::TEST_PROGRESS_TIMEOUT_NS,
        || Ok::<_, Error>(vm_usage_released(&domain)),
    )? {
        return Err(Error::ProgressTimeout);
    }
    Ok(())
}

fn wait_for_vm_usage_release(
    domain: &crate::kernel::accounting::ResourceDomain,
) -> Result<(), Error> {
    if !crate::kernel::task::wait_for_test_progress(
        crate::kernel::task::TEST_PROGRESS_TIMEOUT_NS,
        || Ok::<_, Error>(vm_usage_released(domain)),
    )? {
        return Err(Error::ProgressTimeout);
    }
    Ok(())
}

fn vm_usage_released(domain: &crate::kernel::accounting::ResourceDomain) -> bool {
    use crate::kernel::accounting::ResourceKind;

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
fn test_platform() -> (u64, hyper::vm::interrupt::VirtualInterruptId, u32) {
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
        crate::kernel::vm::device::default_timer_interrupt(),
        profile as u32,
    )
}
