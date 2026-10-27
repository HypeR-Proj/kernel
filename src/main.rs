// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

#![no_std]
#![no_main]

extern crate alloc;

use hyper_hal as hal;
pub use hyper_trace::tracepoint;
pub(crate) mod abi;
pub(crate) mod accounting;
pub(crate) mod authority;
pub(crate) mod block;
pub(crate) mod boot;
pub(crate) mod capability;
pub mod cpu;
pub mod device;
pub(crate) mod entry;
#[cfg(not(all(feature = "kernel-test", feature = "kernel-self-test")))]
pub(crate) mod init;
pub(crate) mod ipc;
pub mod irq;
pub mod mm;
pub(crate) mod object;
pub(crate) mod process;
pub mod random;
pub(crate) mod reaper;
pub mod sync;
pub mod task;
pub mod time;
pub mod vm;

pub mod diagnostics;
pub mod fs;

#[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
#[path = "../tests/kernel/mod.rs"]
mod kernel_tests;

use core::convert::Infallible;
use core::panic::PanicInfo;

// Keep each startup error's type, stage label, and build condition together.
macro_rules! kernel_start_errors {
    ($($(#[$condition:meta])* $variant:ident($error:ty) => $stage:literal),+ $(,)?) => {
        enum KernelStartError {
            $($(#[$condition])* $variant($error),)+
        }

        $(
            $(#[$condition])*
            impl From<$error> for KernelStartError {
                fn from(error: $error) -> Self {
                    Self::$variant(error)
                }
            }
        )+

        impl core::fmt::Debug for KernelStartError {
            fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
                let (stage, error): (&str, &dyn core::fmt::Debug) = match self {
                    $($(#[$condition])* Self::$variant(error) => ($stage, error),)+
                };
                formatter
                    .debug_struct("KernelStartError")
                    .field("stage", &stage)
                    .field("error", error)
                    .finish()
            }
        }
    };
}

kernel_start_errors! {
    Boot(crate::boot::RuntimeError) => "boot",
    Cpu(crate::cpu::Error) => "cpu",
    Crash(crate::diagnostics::crash::InitializationError) => "crash",
    EarlyCrash(crate::diagnostics::crash::EarlyInitializationError) => "early-crash",
    FileSystem(crate::fs::vfs::InitializationError) => "file-system",
    #[cfg(not(all(feature = "kernel-test", feature = "kernel-self-test")))]
    Init(crate::init::Error) => "init",
    Debug(crate::diagnostics::debug::InitializationError) => "debug",
    Device(crate::device::InitializationError) => "device",
    Interrupt(crate::irq::InitializationError) => "interrupt",
    Log(crate::diagnostics::log::InitializationError) => "log",
    Memory(crate::mm::InitializationError) => "memory",
    MemorySealing(crate::mm::FinalizationError) => "memory-sealing",
    Scheduler(crate::task::scheduler::Error) => "scheduler",
    SchedulerBalancing(crate::time::Error) => "scheduler-balancing",
    Time(crate::time::InitializationError) => "time",
    #[cfg(feature = "kernel-trace")]
    Trace(hyper_trace::Error) => "trace",
    VirtualMachineInitialization(crate::vm::InitializationError) => "virtual-machine-initialization",
}

/// Primary kernel entry after architecture initialization is complete.
///
/// Every architecture enters here only after relocation, permanent stage-1
/// translation, runtime exception entry, and the final kernel stack are active.
#[unsafe(no_mangle)]
extern "C" fn start_kernel() -> ! {
    let result: Result<Infallible, KernelStartError> = (|| {
        let mut boot = crate::boot::enter_runtime()?;
        crate::diagnostics::crash::early_initialize()?;

        crate::device::early_initialize(&boot)?;

        crate::mm::initialize()?;
        crate::fs::vfs::initialize(&mut boot)?;
        crate::diagnostics::debug::initialize()?;
        crate::task::initialize()?;
        #[cfg(all(CONFIG_ARCH_AARCH64, feature = "pmu-profiling"))]
        crate::diagnostics::profile::initialize_local_cpu();
        crate::reaper::initialize()?;

        crate::irq::initialize(&mut boot)?;
        crate::reaper::enable_irq_prompts();
        crate::diagnostics::crash::initialize(&boot)?;
        crate::time::initialize(&mut boot)?;
        crate::diagnostics::log::initialize()?;
        crate::fs::cache::worker::initialize()?;
        crate::fs::vfs::read_ahead::initialize()?;
        #[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
        crate::kernel_tests::verify_early_startup();
        crate::cpu::initialize()?;
        crate::time::initialize_workers()?;
        #[cfg(feature = "kernel-trace")]
        crate::diagnostics::trace::initialize()?;
        crate::task::scheduler::initialize_load_balancing()?;
        crate::mm::activate_local_allocator_caches()?;
        crate::mm::seal_address_space()?;

        crate::device::platform_device_initialize(&boot)?;
        crate::vm::initialize(&boot)?;

        #[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
        crate::kernel_tests::run();

        crate::diagnostics::log::report_startup_state();

        #[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
        {
            crate::pr_info!("HypeR test: kernel self-tests completed");
            crate::task::scheduler::exit_current()
        }
        #[cfg(not(all(feature = "kernel-test", feature = "kernel-self-test")))]
        {
            let never = crate::init::start()?;
            match never {}
        }
    })();

    match result {
        Ok(never) => match never {},
        Err(error) => crate::boot::fail("kernel startup", error),
    }
}

/// Rust kernel entry used by secondary CPUs after architectural setup.
#[unsafe(no_mangle)]
extern "C" fn start_secondary_cpu(cpu_index: usize) -> ! {
    hal::cpu::prepare_secondary_entry();
    if !crate::hal::cpu::secondary_is_compatible() {
        crate::hal::cpu::halt()
    }
    crate::hal::memory::enable_local_protection();
    if !crate::hal::memory::local_protection_enabled() {
        crate::hal::cpu::halt()
    }
    crate::cpu::secondary_entry(cpu_index)
}

#[panic_handler]
fn panic(info: &PanicInfo<'_>) -> ! {
    crate::diagnostics::crash::panic(info)
}

/// Assembly-to-Rust boot handoff, before permanent kernel mappings exist.
#[unsafe(no_mangle)]
unsafe extern "C" fn hyper_bootstrap(raw0: usize, raw1: usize, ticks: u64) -> ! {
    // SAFETY: Each selected assembly entry establishes the HAL boot contract.
    let inputs = unsafe { hal::platform::prepare_boot(raw0, raw1, ticks) };
    crate::boot::prepare_boot_environment(inputs)
}
