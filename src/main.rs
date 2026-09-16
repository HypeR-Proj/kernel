// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

#![no_std]
#![no_main]

extern crate alloc;

mod arch;
#[path = "hal/selected/mod.rs"]
mod hal;
pub mod kernel;

#[cfg(feature = "kernel-self-test")]
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
    Boot(crate::kernel::boot::RuntimeError) => "boot",
    Cpu(crate::kernel::cpu::Error) => "cpu",
    Crash(crate::kernel::crash::InitializationError) => "crash",
    EarlyCrash(crate::kernel::crash::EarlyInitializationError) => "early-crash",
    FileSystem(crate::kernel::vfs::InitializationError) => "file-system",
    #[cfg(not(feature = "kernel-self-test"))]
    Init(crate::kernel::init::Error) => "init",
    Debug(crate::kernel::debug::InitializationError) => "debug",
    Device(crate::kernel::device::InitializationError) => "device",
    Interrupt(crate::kernel::irq::InitializationError) => "interrupt",
    Log(crate::kernel::log::InitializationError) => "log",
    Memory(crate::kernel::mm::InitializationError) => "memory",
    MemorySealing(crate::kernel::mm::FinalizationError) => "memory-sealing",
    Scheduler(crate::kernel::task::scheduler::Error) => "scheduler",
    Time(crate::kernel::time::InitializationError) => "time",
    VirtualMachineInitialization(crate::kernel::vm::InitializationError) => "virtual-machine-initialization",
}

/// Primary kernel entry after architecture initialization is complete.
///
/// Every architecture enters here only after relocation, permanent stage-1
/// translation, runtime exception entry, and the final kernel stack are active.
#[unsafe(no_mangle)]
extern "C" fn start_kernel() -> ! {
    let result: Result<Infallible, KernelStartError> = (|| {
        let mut boot = crate::kernel::boot::enter_runtime()?;
        crate::kernel::crash::early_initialize()?;

        crate::kernel::device::early_initialize(&boot)?;

        crate::kernel::mm::initialize()?;
        crate::kernel::vfs::initialize(&boot)?;
        crate::kernel::debug::initialize()?;
        crate::kernel::task::initialize()?;
        crate::kernel::reaper::initialize()?;

        crate::kernel::irq::initialize(&mut boot)?;
        crate::kernel::reaper::enable_irq_prompts();
        crate::kernel::crash::initialize(&boot)?;
        crate::kernel::time::initialize(&mut boot)?;
        crate::kernel::log::initialize()?;
        #[cfg(feature = "kernel-self-test")]
        crate::kernel_tests::verify_early_startup();
        crate::kernel::cpu::initialize()?;
        crate::kernel::mm::activate_local_allocator_caches()?;
        crate::kernel::mm::seal_address_space()?;

        crate::kernel::device::platform_device_initialize(&boot)?;
        crate::kernel::vm::initialize(&boot)?;
        crate::kernel::debug::report_startup_state();

        #[cfg(feature = "kernel-self-test")]
        crate::kernel_tests::run();

        crate::kernel::log::report_startup_state();

        #[cfg(feature = "kernel-self-test")]
        {
            crate::pr_info!("HypeR test: kernel self-tests completed");
            crate::kernel::task::scheduler::exit_current()
        }
        #[cfg(not(feature = "kernel-self-test"))]
        {
            let never = crate::kernel::init::start()?;
            match never {}
        }
    })();

    match result {
        Ok(never) => match never {},
        Err(error) => crate::kernel::boot::fail("kernel startup", error),
    }
}

/// Rust kernel entry used by secondary CPUs after architectural setup.
#[unsafe(no_mangle)]
extern "C" fn start_secondary_cpu(cpu_index: usize) -> ! {
    if !crate::hal::cpu::secondary_is_compatible() {
        crate::hal::cpu::halt()
    }
    crate::hal::memory::enable_local_protection();
    if !crate::hal::memory::local_protection_enabled() {
        crate::hal::cpu::halt()
    }
    crate::kernel::cpu::secondary_entry(cpu_index)
}

#[panic_handler]
fn panic(info: &PanicInfo<'_>) -> ! {
    crate::kernel::crash::panic(info)
}
