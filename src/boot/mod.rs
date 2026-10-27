// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

use core::convert::Infallible;
use hyper_drivers::serial::console;
use {hyper_drivers::platform::MmioResource, hyper_drivers::platform::PermanentMmioMapping};
use {hyper_platform::ConsoleInfo, hyper_platform::chosen, hyper_platform::fdt};

use self::state::BootState;
use {crate::diagnostics::log, crate::irq, crate::mm, crate::time};

pub(crate) mod image;
mod state;

/// Borrows arguments left after kernel boot processing. Userspace policy
/// interprets these bytes; this accessor neither selects services nor profiles.
#[cfg(not(all(feature = "kernel-test", feature = "kernel-self-test")))]
pub(crate) fn with_command_line<R>(read: impl FnOnce(Option<&chosen::CommandLine>) -> R) -> R {
    match state::with(|state| read(state.command_line.as_ref())) {
        Ok(value) => value,
        Err(_) => hyper::debug::invariant_failure("boot command line before state publication"),
    }
}

pub enum ConsoleError {
    Address(u64),
    Driver(console::BindError),
}

pub enum PreparationError {
    BootState(state::Error),
    Chosen(chosen::Error),
    Console(ConsoleError),
    Essential(crate::hal::platform::DiscoveryError),
    InitialRamdiskInvalid(hyper::mm::ranges::PhysicalRange),
    InitialRamdiskMissing,
    Kaslr(crate::hal::platform::KaslrError),
    MemoryActivationUnavailable,
    Memory(mm::memory::Error),
    Platform(fdt::Error),
}

impl core::fmt::Debug for PreparationError {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let (stage, error): (&str, Option<&dyn core::fmt::Debug>) = match self {
            Self::BootState(error) => ("boot-state", Some(error)),
            Self::Chosen(error) => ("chosen", Some(error)),
            Self::Console(error) => ("console", Some(error)),
            Self::Essential(error) => ("essential-devices", Some(error)),
            Self::InitialRamdiskInvalid(range) => ("initial-ramdisk", Some(range)),
            Self::InitialRamdiskMissing => ("initial-ramdisk-missing", None),
            Self::Kaslr(error) => ("kaslr", Some(error)),
            Self::MemoryActivationUnavailable => ("memory-activation-unavailable", None),
            Self::Memory(error) => ("memory", Some(error)),
            Self::Platform(error) => ("platform", Some(error)),
        };
        format_error(formatter, "PreparationError", stage, error)
    }
}

pub enum RuntimeError {
    Cache(hyper::hal::cache::CacheError),
    CpuAdmission,
    Console(ConsoleError),
    DtbMapping(u64),
    InitialRamdiskAddress(hyper::mm::ranges::PhysicalRange),
    InitialRamdiskSize(hyper::mm::ranges::PhysicalRange),
}

impl core::fmt::Debug for ConsoleError {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Address(address) => {
                format_error(formatter, "ConsoleError", "address", Some(address))
            }
            Self::Driver(error) => format_error(formatter, "ConsoleError", "driver", Some(error)),
        }
    }
}

impl core::fmt::Debug for RuntimeError {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Cache(error) => format_error(formatter, "RuntimeError", "cache", Some(error)),
            Self::CpuAdmission => format_error(formatter, "RuntimeError", "cpu-admission", None),
            Self::Console(error) => format_error(formatter, "RuntimeError", "console", Some(error)),
            Self::DtbMapping(address) => {
                format_error(formatter, "RuntimeError", "dtb-mapping", Some(address))
            }
            Self::InitialRamdiskAddress(range) => format_error(
                formatter,
                "RuntimeError",
                "initial-ramdisk-address",
                Some(range),
            ),
            Self::InitialRamdiskSize(range) => format_error(
                formatter,
                "RuntimeError",
                "initial-ramdisk-size",
                Some(range),
            ),
        }
    }
}

fn format_error(
    formatter: &mut core::fmt::Formatter<'_>,
    kind: &str,
    stage: &str,
    error: Option<&dyn core::fmt::Debug>,
) -> core::fmt::Result {
    let mut report = formatter.debug_struct(kind);
    report.field("stage", &stage);
    if let Some(error) = error {
        report.field("error", error);
    }
    report.finish()
}

use crate::hal::platform::ProtocolInputs;

mod archive;
pub(crate) use archive::Archive;

/// State shared only between the top-level kernel initialization steps.
///
/// Keeping this type small makes dependencies between subsystem entry points
/// explicit without exposing the full early-boot state to every subsystem.
pub(crate) struct Initialization {
    essential: crate::hal::platform::EssentialInfo,
    early_console: Option<ConsoleInfo>,
    early_console_mapping: Option<PermanentMmioMapping>,
    linear_dtb: usize,
    initial_ramdisk: Option<Archive>,
    interrupts: Option<irq::interrupt::Capabilities>,
    timer: Option<time::Capabilities>,
}

impl Initialization {
    pub(crate) fn maps_mmio(&self, resource: MmioResource) -> bool {
        with_boot_state(|state| {
            state
                .platform
                .mmio
                .as_slice()
                .iter()
                .any(|mapped| mapped.start() <= resource.start() && resource.end() <= mapped.end())
        })
    }

    pub(crate) fn essential(&self) -> &crate::hal::platform::EssentialInfo {
        &self.essential
    }

    pub(crate) const fn linear_dtb(&self) -> usize {
        self.linear_dtb
    }

    pub(crate) const fn early_console(&self) -> Option<ConsoleInfo> {
        self.early_console
    }

    /// Returns the permanent MMIO capability installed while promoting
    /// earlycon. Port-I/O consoles do not have an MMIO capability.
    pub(crate) const fn early_console_mapping(&self) -> Option<PermanentMmioMapping> {
        self.early_console_mapping
    }

    pub(crate) fn take_initial_ramdisk(&mut self) -> Option<Archive> {
        self.initial_ramdisk.take()
    }

    pub(crate) fn interrupts(&self) -> irq::interrupt::Capabilities {
        self.interrupts
            .unwrap_or_else(|| fail("interrupt capability access", "controller not initialized"))
    }

    pub(crate) fn set_interrupts(&mut self, capabilities: irq::interrupt::Capabilities) {
        self.interrupts = Some(capabilities);
    }

    pub(crate) fn timer(&self) -> time::Capabilities {
        self.timer
            .unwrap_or_else(|| fail("timer capability access", "timer not initialized"))
    }

    pub(crate) fn set_timer(&mut self, capabilities: time::Capabilities) {
        self.timer = Some(capabilities);
    }
}

/// Prepares kernel state while executing on the bootstrap mapping.
pub(crate) fn prepare_boot_environment(inputs: ProtocolInputs) -> ! {
    match try_prepare_boot_environment(inputs) {
        Ok(never) => match never {},
        Err(error) => fail("boot environment preparation", error),
    }
}

fn try_prepare_boot_environment(inputs: ProtocolInputs) -> Result<Infallible, PreparationError> {
    let dtb_address = inputs.dtb_address;
    let image_layout = image::layout();
    let (platform, essential, chosen) = discover_boot_inputs(dtb_address)?;
    report_chosen_errors(&chosen);
    let command_line = inputs.command_line.as_ref().or(chosen.command_line());
    let early_console = initialize_early_console(&platform, command_line)?;
    crate::pr_info!("HypeR: DTB at {:#x}", dtb_address);

    let hardware_seed = crate::hal::platform::random_seed(essential.cpu_power());
    crate::random::initialize(chosen.kaslr_seed(), chosen.rng_seed(), hardware_seed);
    // Timing alone is sufficient for the general non-security PRNG, but does
    // not silently enable KASLR without a firmware or hardware seed.
    let has_seed =
        chosen.kaslr_seed().is_some() || chosen.rng_seed().is_some() || hardware_seed.is_some();
    let kernel_base = select_kernel_base(
        command_line,
        has_seed.then(crate::random::next_u64),
        image_layout.total_size,
    )?;
    let initial_ramdisk = select_initial_ramdisk(
        &platform,
        inputs.initial_ramdisk.or(chosen.initial_ramdisk()),
        dtb_address,
        image_layout,
    )?;
    let mut memory = prepare_final_memory(&platform, dtb_address, initial_ramdisk, kernel_base)?;
    let activation = memory
        .take_activation_context()
        .ok_or(PreparationError::MemoryActivationUnavailable)?;

    crate::pr_info!(
        "HypeR: discovered {} RAM and {} MMIO regions",
        platform.memory.len(),
        platform.mmio.len()
    );
    crate::pr_info!(
        "HypeR: root page table at {:#x}; switching to kernel VA {:#x}",
        memory.root_address(),
        memory.kernel_base()
    );

    state::install(BootState {
        platform,
        essential,
        early_console,
        #[cfg(not(all(feature = "kernel-test", feature = "kernel-self-test")))]
        command_line: command_line.map(|line| line.without_arguments(&["earlycon", "nokaslr"])),
        memory,
        dtb_address: dtb_address as u64,
        image_physical_start: image_layout.physical_start,
        initial_ramdisk,
    })
    .map_err(PreparationError::BootState)?;

    // SAFETY: The prepared hierarchy is retained in immutable global boot
    // state. `activation` is its unique transition token and cannot be issued
    // again after that publication.
    unsafe { crate::hal::memory::activate(activation) }
}

fn discover_boot_inputs(
    dtb_address: usize,
) -> Result<
    (
        hyper_platform::PlatformInfo,
        crate::hal::platform::EssentialInfo,
        chosen::Properties,
    ),
    PreparationError,
> {
    let mut essential_discovery = crate::hal::platform::EssentialDiscovery::new();
    let mut chosen_discovery = chosen::Discovery::new();
    let mut visitors = fdt::VisitorPair::new(&mut essential_discovery, &mut chosen_discovery);
    // SAFETY: The architecture bootstrap preserved the validated firmware DTB
    // pointer until this common discovery phase.
    let platform = match unsafe { fdt::discover_with(dtb_address, &mut visitors) } {
        Ok(platform) => platform,
        Err(fdt::WalkError::Fdt(error)) => return Err(PreparationError::Platform(error)),
        Err(fdt::WalkError::Visitor(fdt::VisitorPairError::First(error))) => {
            return Err(PreparationError::Essential(error));
        }
        Err(fdt::WalkError::Visitor(fdt::VisitorPairError::Second(error))) => {
            return Err(PreparationError::Chosen(error));
        }
    };
    let essential = essential_discovery
        .finish()
        .map_err(PreparationError::Essential)?;
    let chosen = chosen_discovery
        .finish()
        .map_err(PreparationError::Chosen)?;

    Ok((platform, essential, chosen))
}

fn report_chosen_errors(chosen: &chosen::Properties) {
    if let Some(error) = chosen.command_line_error() {
        crate::pr_warn!("HypeR: ignoring invalid DTB bootargs: {error:?}");
    }
    if let Some(error) = chosen.rng_seed_error() {
        crate::pr_warn!("HypeR: ignoring invalid DTB rng-seed: {error:?}");
    }
    if let Some(error) = chosen.kaslr_seed_error() {
        crate::pr_warn!("HypeR: ignoring invalid DTB kaslr-seed: {error:?}");
    }
}

fn initialize_early_console(
    platform: &hyper_platform::PlatformInfo,
    command_line: Option<&chosen::CommandLine>,
) -> Result<Option<ConsoleInfo>, PreparationError> {
    let early_console = match console::early_console(command_line) {
        Ok(Some(info)) if early_console_is_accessible(platform, info) => Some(info),
        Ok(Some(info)) => {
            crate::pr_warn!(
                "HypeR: ignoring inaccessible early console at {:#x}",
                info.base
            );
            None
        }
        Ok(None) => None,
        Err(error) => {
            crate::pr_warn!("HypeR: ignoring invalid earlycon argument: {error:?}");
            None
        }
    };

    if let Some(console_info) = early_console {
        install_bootstrap_console(console_info).map_err(PreparationError::Console)?;
        crate::pr_info!("HypeR: early console initialized");
        crate::pr_info!(
            "HypeR version {}-{} (built {})",
            env!("CARGO_PKG_VERSION"),
            env!("HYPER_BUILD_REVISION"),
            env!("HYPER_BUILD_TIMESTAMP")
        );
    }
    Ok(early_console)
}

fn select_kernel_base(
    command_line: Option<&chosen::CommandLine>,
    discovered_seed: Option<u64>,
    image_size: u64,
) -> Result<u64, PreparationError> {
    let kaslr_seed = if cfg!(CONFIG_RANDOMIZE_BASE)
        && !command_line.is_some_and(|arguments| arguments.contains("nokaslr"))
    {
        discovered_seed
    } else {
        None
    };
    crate::hal::platform::select_kernel_base(kaslr_seed, image_size)
        .map_err(PreparationError::Kaslr)
}

fn select_initial_ramdisk(
    platform: &hyper_platform::PlatformInfo,
    discovered: Option<hyper::mm::ranges::PhysicalRange>,
    dtb_address: usize,
    image_layout: hyper::hal::memory::KernelImageLayout,
) -> Result<hyper::mm::ranges::PhysicalRange, PreparationError> {
    let range = discovered.ok_or(PreparationError::InitialRamdiskMissing)?;
    if !initial_ramdisk_is_accessible(platform, range, dtb_address as u64, image_layout) {
        return Err(PreparationError::InitialRamdiskInvalid(range));
    }
    Ok(range)
}

fn prepare_final_memory(
    platform: &hyper_platform::PlatformInfo,
    dtb_address: usize,
    initial_ramdisk: hyper::mm::ranges::PhysicalRange,
    kernel_base: u64,
) -> Result<mm::PreparedMemory, PreparationError> {
    // SAFETY: The architecture established and owns the bootstrap mapping used
    // by its early physical-page allocation policy.
    unsafe { mm::memory::prepare(platform, dtb_address as u64, initial_ramdisk, kernel_base) }
        .map_err(PreparationError::Memory)
}

/// Enters architecture-independent initialization on the permanent mappings.
pub(crate) fn enter_runtime() -> Result<Initialization, RuntimeError> {
    let (platform, essential, early_console, dtb_address, initial_ramdisk) =
        with_boot_state(|state| {
            (
                state.platform,
                state.essential,
                state.early_console,
                state.dtb_address,
                state.initial_ramdisk,
            )
        });
    let early_console_mapping = match early_console {
        Some(console_info) => {
            // The bootstrap identity map has already been retired. Remove its
            // stale handle before any fallible runtime operation can log, then
            // publish only a handle backed by the permanent device mapping.
            log::console::retire_bootstrap();
            promote_early_console(&platform, console_info).map_err(RuntimeError::Console)?
        }
        None => None,
    };
    if !crate::hal::cpu::prepare_primary_admission() {
        return Err(RuntimeError::CpuAdmission);
    }
    crate::hal::cache::prepare(&essential).map_err(RuntimeError::Cache)?;
    let linear_dtb =
        mm::memory::linear_address(dtb_address).ok_or(RuntimeError::DtbMapping(dtb_address))?;
    let initial_ramdisk = map_initial_ramdisk(initial_ramdisk)?;
    let pages = with_boot_state(|state| state.memory.take_archive_pages())
        .unwrap_or_else(|| fail("archive ownership", "reservation already transferred"));

    Ok(Initialization {
        essential,
        early_console,
        early_console_mapping,
        linear_dtb,
        initial_ramdisk: Some(Archive::new(initial_ramdisk, pages)),
        interrupts: None,
        timer: None,
    })
}

fn map_initial_ramdisk(
    range: hyper::mm::ranges::PhysicalRange,
) -> Result<&'static [u8], RuntimeError> {
    let address = mm::memory::linear_address(range.start())
        .ok_or(RuntimeError::InitialRamdiskAddress(range))?;
    let size =
        usize::try_from(range.size()).map_err(|_| RuntimeError::InitialRamdiskSize(range))?;
    if size > isize::MAX as usize || address.checked_add(size).is_none() {
        return Err(RuntimeError::InitialRamdiskSize(range));
    }
    // SAFETY: Early boot validated and reserved the complete firmware-owned
    // range before allocator handoff. The permanent linear map covers it.
    // The only caller immediately puts this slice into the one-shot Archive
    // owner; no public API exposes the artificially extended slice lifetime.
    Ok(unsafe { core::slice::from_raw_parts(core::ptr::with_exposed_provenance(address), size) })
}

fn initial_ramdisk_is_accessible(
    platform: &hyper_platform::PlatformInfo,
    range: hyper::mm::ranges::PhysicalRange,
    dtb_address: u64,
    image: hyper::hal::memory::KernelImageLayout,
) -> bool {
    let in_ram = platform
        .memory
        .as_slice()
        .iter()
        .any(|memory| memory.start() <= range.start() && range.end() <= memory.end());
    let excluded = platform
        .no_map
        .as_slice()
        .iter()
        .any(|reserved| reserved.overlaps(range));
    let overlaps_dtb = hyper::mm::ranges::PhysicalRange::new(dtb_address, platform.dtb_size)
        .is_none_or(|dtb| dtb.overlaps(range));
    let overlaps_image =
        hyper::mm::ranges::PhysicalRange::new(image.physical_start, image.total_size)
            .is_none_or(|kernel| kernel.overlaps(range));
    in_ram && !excluded && !overlaps_dtb && !overlaps_image
}

fn early_console_is_accessible(platform: &hyper_platform::PlatformInfo, info: ConsoleInfo) -> bool {
    if info.access == hyper_platform::ConsoleRegisterAccess::Port {
        return info.base <= u64::from(u16::MAX - 7) && crate::hal::platform::port_io().is_some();
    }
    let Some(register_window) = early_console_register_window(info) else {
        return false;
    };
    let Some(end) = info.base.checked_add(register_window) else {
        return false;
    };
    if end > crate::hal::memory::bootstrap_accessible_limit() {
        return false;
    }
    platform
        .mmio
        .as_slice()
        .iter()
        .any(|range| range.start() <= info.base && end <= range.end())
}

fn early_console_mmio_resource(
    platform: &hyper_platform::PlatformInfo,
    info: ConsoleInfo,
) -> Option<MmioResource> {
    let size = early_console_register_window(info)?;
    let resource = hyper::mm::ranges::PhysicalRange::new(info.base, size)?;
    platform
        .mmio
        .as_slice()
        .iter()
        .any(|range| range.start() <= resource.start() && resource.end() <= range.end())
        .then(|| {
            // SAFETY: Discovery classified the containing translated interval
            // as MMIO, and earlycon policy assigns this exact UART window to
            // the selected console.
            unsafe { MmioResource::from_physical_range(resource) }
        })
}

fn early_console_register_window(info: ConsoleInfo) -> Option<u64> {
    match info.kind {
        hyper_platform::ConsoleKind::Pl011 => {
            Some(hyper_drivers::serial::Pl011::REGISTER_WINDOW_SIZE)
        }
        hyper_platform::ConsoleKind::Ns16550 => match info.access {
            hyper_platform::ConsoleRegisterAccess::Mmio8 { register_shift } => 1u64
                .checked_shl(u32::from(register_shift))
                .and_then(|stride| 7u64.checked_mul(stride))
                .and_then(|last| last.checked_add(1)),
            hyper_platform::ConsoleRegisterAccess::Mmio32 { register_shift } => 1u64
                .checked_shl(u32::from(register_shift))
                .and_then(|stride| 7u64.checked_mul(stride))
                .and_then(|last| last.checked_add(core::mem::size_of::<u32>() as u64)),
            hyper_platform::ConsoleRegisterAccess::Native => Some(8),
            hyper_platform::ConsoleRegisterAccess::Port => None,
        },
    }
}

fn install_bootstrap_console(console_info: ConsoleInfo) -> Result<(), ConsoleError> {
    let base =
        usize::try_from(console_info.base).map_err(|_| ConsoleError::Address(console_info.base))?;
    // SAFETY: Platform discovery validated the device range and the
    // architecture bootstrap maps it with Device memory attributes. This
    // handle is replaced immediately after the permanent stage-1 switch.
    let console =
        unsafe { console::bind_bootstrap(console_info, base, crate::hal::platform::port_io()) }
            .map_err(ConsoleError::Driver)?;
    log::console::install(console);
    Ok(())
}

/// Replaces the bootstrap identity-mapped console handle with one backed by
/// the permanent stage-1 capability. Runtime serial input borrows this same
/// mapping, so promotion is the single mapping publication point.
fn promote_early_console(
    platform: &hyper_platform::PlatformInfo,
    console_info: ConsoleInfo,
) -> Result<Option<PermanentMmioMapping>, ConsoleError> {
    let mapping = if console_info.access == hyper_platform::ConsoleRegisterAccess::Port {
        None
    } else {
        let resource = early_console_mmio_resource(platform, console_info)
            .ok_or(ConsoleError::Address(console_info.base))?;
        let virtual_start = mm::memory::mmio_address(resource.start())
            .ok_or(ConsoleError::Address(resource.start()))?;
        // SAFETY: Final stage-1 construction permanently maps every platform
        // MMIO range with Device attributes.
        Some(
            unsafe {
                PermanentMmioMapping::new(
                    resource,
                    hyper::mm::VirtualAddress::new(virtual_start as u64),
                )
            }
            .map_err(|_| ConsoleError::Address(resource.start()))?,
        )
    };
    let console = console::bind(console_info, mapping, crate::hal::platform::port_io())
        .map_err(ConsoleError::Driver)?;
    log::console::install(console);
    Ok(mapping)
}

pub(crate) fn fail(operation: &str, error: impl core::fmt::Debug) -> ! {
    if crate::diagnostics::crash::is_ready() {
        crate::diagnostics::crash::fatal(format_args!("HypeR: {operation} failed: {error:?}"));
    }
    crate::pr_crit!("HypeR: {operation} failed: {error:?}");
    crate::hal::cpu::halt()
}

pub(crate) fn with_boot_state<R>(operation: impl FnOnce(&BootState) -> R) -> R {
    match state::with(operation) {
        Ok(result) => result,
        Err(error) => fail("boot-state access", error),
    }
}

pub(crate) fn try_with_boot_state<R>(operation: impl FnOnce(&BootState) -> R) -> Option<R> {
    state::with(operation).ok()
}
