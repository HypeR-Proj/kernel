// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

use super::{DEPTH, INFO_SIZE, Result, SAMPLE_SIZE};
use crate::hal::irq::{InterruptFrame, InterruptedRegisters};
use crate::hal::pmu;
use core::ptr::addr_of;
use hyper::abi::native::*;
use hyper::cpu::{CpuIndex, MAX_CPUS, PerCpu};
use hyper::hal::interrupt::{InterruptId, InterruptPriority, InterruptTrigger};
use hyper::sync::{
    InterruptMaskGuard, InterruptSpinLock, PublishedOnce,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use hyper_drivers::platform::PlatformDevice;
use {crate::irq::interrupt::HandlerResult, crate::irq::interrupt::VirtualInterrupt};

use {crate::time, crate::time::Callout, crate::time::CalloutRef, crate::time::TimerEvent};

const CAPACITY: usize = 256;
type Lock<T> = InterruptSpinLock<T, crate::hal::irq::LocalMask>;
#[derive(Clone, Copy)]
struct Sample([u64; 24]);
struct Buffer {
    samples: [Sample; CAPACITY],
    count: u64,
}
impl Buffer {
    const fn new() -> Self {
        Self {
            samples: [Sample([0; 24]); CAPACITY],
            count: 0,
        }
    }
}
struct Config {
    generation: u64,
    period: u64,
    deadline: u64,
}
static CONTROL: Lock<Config> = Lock::new(Config {
    generation: 0,
    period: 0,
    deadline: 0,
});
static BUFFERS: PerCpu<Lock<Buffer>> = PerCpu::new([const { Lock::new(Buffer::new()) }; MAX_CPUS]);
static ADMISSION: super::admission::Admission<MAX_CPUS> = super::admission::Admission::new();
static PERIOD: AtomicU64 = AtomicU64::new(0);
static SUPPORTED: PerCpu<AtomicBool> = PerCpu::new([const { AtomicBool::new(false) }; MAX_CPUS]);
static DROPPED: PerCpu<AtomicU64> = PerCpu::new([const { AtomicU64::new(0) }; MAX_CPUS]);
static READY: AtomicBool = AtomicBool::new(false);
static EXPIRY: PublishedOnce<Callout<()>> = PublishedOnce::new();
unsafe extern "C" {
    static __image_start: u8;
    static __rodata_start: u8;
}

pub(crate) fn owns_node(node: &PlatformDevice) -> bool {
    node.is_compatible("arm,armv8-pmuv3")
        || node.is_compatible("arm,cortex-a76-pmu")
        || node.is_compatible("arm,cortex-a57-pmu")
        || node.is_compatible("arm,cortex-a72-pmu")
}

/// Takes local PMU ownership once, before this CPU enables interrupts. Discovery
/// and route activation happen later, after all admitted CPUs have quiesced
/// inherited counter state. Unsupported CPUs never access PMU registers.
pub(crate) fn initialize_local_cpu() {
    if let Some(cpu) = crate::cpu::current_index()
        && pmu::available()
    {
        pmu::initialize();
        SUPPORTED[cpu].store(true, Ordering::Release);
    }
}

/// Discovers the firmware PPI or per-CPU SPI routes. No guessed interrupt numbers.
/// Partial failure leaves counters disabled and profiling unavailable.
pub(crate) fn initialize(nodes: &[PlatformDevice], domain: crate::irq::interrupt::IrqDomainId) {
    if let Err(error) = install(nodes, domain) {
        crate::pr_warn!("HypeR: PMU profiling unavailable: {error}");
    }
}
fn install(
    nodes: &[PlatformDevice],
    domain: crate::irq::interrupt::IrqDomainId,
) -> core::result::Result<(), &'static str> {
    use crate::irq::interrupt;
    let mut pmus = nodes.iter().filter(|node| owns_node(node));
    let node = pmus.next().ok_or("no supported firmware PMU")?;
    if pmus.next().is_some() {
        return Err("multiple PMU nodes are unsupported");
    }
    let cells = node.interrupt_cells();
    let n = crate::cpu::online_cpu_count();
    if n == 0 || n > 64 {
        return Err("more than 64 CPUs");
    }
    if SUPPORTED.iter().take(n).any(|v| !v.load(Ordering::Acquire)) {
        return Err("PMUv3 unavailable on an admitted CPU");
    }
    let mut routes = [0u32; MAX_CPUS];
    if cells.len() == 3 && cells[0] == 1 {
        let route = crate::hal::irq::decode_platform(cells).map_err(|_| "invalid PMU PPI")?;
        if route.trigger != hyper_platform::PlatformInterruptTrigger::Level {
            return Err("PMU requires level interrupts");
        }
        routes[..n].fill(route.interrupt);
    } else {
        let affinity = node
            .property("interrupt-affinity")
            .ok_or("missing SPI affinity")?;
        if cells.len() % 3 != 0 || affinity.len() != cells.len() / 3 * 4 {
            return Err("invalid SPI affinity");
        }
        let boot = crate::hal::cpu::current_hardware_id().get();
        let topology = crate::boot::with_boot_state(|state| state.platform.cpus);
        let mut ids = [0u64; MAX_CPUS];
        ids[0] = boot;
        let mut next = 1;
        for cpu in topology
            .as_slice()
            .iter()
            .filter(|cpu| cpu.hardware_id != boot)
        {
            if next < n {
                ids[next] = cpu.hardware_id;
                next += 1;
            }
        }
        for (spec, phandle) in cells.chunks_exact(3).zip(affinity.chunks_exact(4)) {
            if spec[0] != 0 {
                return Err("mixed PMU interrupt types");
            }
            let cpu_node = nodes
                .iter()
                .find(|candidate| candidate.property("phandle") == Some(phandle))
                .ok_or("unknown CPU phandle")?;
            let reg = cpu_node.property("reg").ok_or("missing CPU reg")?;
            if !matches!(reg.len(), 4 | 8) {
                return Err("invalid CPU reg");
            }
            let hardware = reg
                .iter()
                .fold(0u64, |value, byte| (value << 8) | u64::from(*byte));
            let Some(index) = ids[..n].iter().position(|id| *id == hardware) else {
                continue;
            };
            let route = crate::hal::irq::decode_platform(spec).map_err(|_| "invalid PMU SPI")?;
            if route.trigger != hyper_platform::PlatformInterruptTrigger::Level {
                return Err("PMU requires level interrupts");
            }
            if routes[index] != 0 {
                return Err("duplicate PMU CPU");
            }
            routes[index] = route.interrupt;
        }
        if routes[..n].contains(&0) {
            return Err("incomplete PMU CPU routes");
        }
    }
    for (index, &id) in routes[..n].iter().enumerate() {
        if id >= 32 && routes[..index].contains(&id) {
            return Err("duplicate PMU SPI");
        }
    }
    let expiry = Callout::try_new((), expire).map_err(|_| "PMU expiry callout allocation")?;
    EXPIRY
        .publish(expiry)
        .map_err(|_| "PMU expiry already installed")?;
    // All local PMUs are disabled before activating the first route. Each
    // registered handler can clear a stale pending overflow even if activation
    // fails partway, but READY never permits an incomplete capture to start.
    for (index, &id) in routes[..n].iter().enumerate() {
        if index == 0 || id >= 32 {
            let prepared = domain
                .prepare_shared_mapping(
                    InterruptId::new(id),
                    InterruptPriority::Low,
                    InterruptTrigger::Level,
                    0,
                    handle_interrupt,
                )
                .map_err(|_| "PMU IRQ registration")?;
            if id >= 32 {
                let cpu = CpuIndex::new(index).ok_or("invalid CPU")?;
                if interrupt::route_prepared_to_cpu(&prepared, cpu).is_err() {
                    if let Err(failure) = interrupt::discard_prepared(prepared) {
                        let (_, retained) = failure.into_parts();
                        core::mem::forget(retained);
                    }
                    return Err("PMU SPI routing");
                }
            }
            match interrupt::activate(prepared) {
                Ok(registration) => registration.retain_permanently(),
                Err(failure) => {
                    let (_, retained) = failure.into_parts();
                    core::mem::forget(retained);
                    return Err("PMU IRQ activation");
                }
            }
        }
    }
    READY.store(true, Ordering::Release);
    crate::pr_info!("HypeR: PMUv3 profiler ready on {n} CPUs (disabled until requested)");
    Ok(())
}

pub(crate) fn start(period: u64, seconds: u64) -> Result<u64> {
    if !(1_000_000..=1_000_000_000).contains(&period) || !(1..=300).contains(&seconds) {
        return Err(HYPER_NATIVE_STATUS_INVALID_ARGUMENT);
    }
    CONTROL.with(|config| {
        if !READY.load(Ordering::Acquire) {
            return Err(HYPER_NATIVE_STATUS_NOT_SUPPORTED);
        }
        if ADMISSION.requested() != 0 || !ADMISSION.quiescent() {
            return Err(HYPER_NATIVE_STATUS_BUSY);
        }
        let generation = config
            .generation
            .checked_add(1)
            .filter(|generation| *generation <= super::admission::MAX_GENERATION)
            .ok_or(HYPER_NATIVE_STATUS_RESOURCE_LIMIT)?;
        let now = crate::time::monotonic_nanoseconds().map_err(|_| HYPER_NATIVE_STATUS_INTERNAL)?;
        let deadline = now
            .checked_add(seconds * 1_000_000_000)
            .ok_or(HYPER_NATIVE_STATUS_INVALID_ARGUMENT)?;
        let timer = EXPIRY.get().ok_or(HYPER_NATIVE_STATUS_NOT_SUPPORTED)?;
        let ticks = time::deadline_from_monotonic_nanoseconds(deadline)
            .map_err(|_| HYPER_NATIVE_STATUS_INTERNAL)?;
        // Prepare expiry before publishing the capture. A previously running
        // callback is serialized by CONTROL and checks this capture's deadline;
        // reset preserves its separately scheduled successor.
        timer
            .reset(ticks)
            .map_err(|_| HYPER_NATIVE_STATUS_INTERNAL)?;
        for buffer in BUFFERS.iter() {
            buffer.with(|buffer| buffer.count = 0);
        }
        for dropped in DROPPED.iter() {
            dropped.store(0, Ordering::Relaxed);
        }
        *config = Config {
            generation,
            period,
            deadline,
        };
        PERIOD.store(period, Ordering::Relaxed);
        ADMISSION.publish(generation);
        if !notify_control() {
            ADMISSION.close(generation);
            let _ = timer.cancel();
            let _ = notify_control();
            return Err(HYPER_NATIVE_STATUS_INTERNAL);
        }
        Ok(generation)
    })
}

/// Prompts every CPU to disarm through the existing kernel IPI. BUSY means stop
/// is requested but an admitted CPU has not acknowledged; callers retry, never
/// spin in the kernel. Late start prompts cannot admit a closed generation.
pub(crate) fn stop(generation: u64) -> Result<()> {
    CONTROL.with(|config| {
        if generation == 0 || generation != config.generation {
            return Err(HYPER_NATIVE_STATUS_BAD_STATE);
        }
        ADMISSION.close(generation);
        let notified = notify_control();
        if let Some(timer) = EXPIRY.get() {
            timer.cancel().map_err(|_| HYPER_NATIVE_STATUS_INTERNAL)?;
        }
        if !notified {
            return Err(HYPER_NATIVE_STATUS_INTERNAL);
        }
        if !ADMISSION.quiescent() {
            Err(HYPER_NATIVE_STATUS_BUSY)
        } else {
            Ok(())
        }
    })
}

/// The normal-context callout never waits for another CPU. Checking the current
/// deadline under CONTROL makes a callback already claimed before cancel/reset
/// harmless to a newer capture. The callout's lifetime is the kernel lifetime.
fn expire(_: &(), _: CalloutRef<'_>, _: TimerEvent) {
    let notified = CONTROL.with(|config| {
        let request = ADMISSION.requested();
        if request == 0 {
            return true;
        }
        if time::monotonic_nanoseconds().is_ok_and(|now| now < config.deadline) {
            return true;
        }
        ADMISSION.close(request);
        notify_control()
    });
    if !notified {
        crate::pr_warn!("HypeR: PMU stop notification failed; capture remains retained");
    }
}

/// Called with IRQs masked (CONTROL or PMU entry). Notifications are coalesced
/// prompts, not commands: each recipient reconciles the latest durable request.
/// No shared RPC payload, transaction lock, acknowledgement spin or self-IPI.
fn notify_control() -> bool {
    let current = crate::cpu::current_index();
    let mut delivered = true;
    for index in 0..crate::cpu::online_cpu_count() {
        if let Some(cpu) = CpuIndex::new(index)
            && Some(cpu) != current
        {
            delivered &= crate::hal::irq::notify_kernel_rpc(
                cpu,
                hyper::hal::interrupt::KernelRpcReasons::PMU_CONTROL,
            );
        }
    }
    service_control();
    delivered
}

/// Bounded local register work, called by IPI dispatch or cooperative RPC polling.
/// It must not take CONTROL: the publisher may hold it while sending the IPI.
pub(crate) fn service_control() {
    // SAFETY: This nonblocking routine stays on the calling CPU. The guard
    // serializes PMU control with overflow handling even during RPC polling
    // outside physical IRQ entry, and restores the exact prior mask state.
    let _mask = unsafe { InterruptMaskGuard::<crate::hal::irq::LocalMask>::acquire() };
    let Some(cpu) = crate::cpu::current_index() else {
        return;
    };
    if !SUPPORTED[cpu].load(Ordering::Acquire) {
        return;
    }
    let request = ADMISSION.requested();
    let owner = ADMISSION.owner(cpu.get());
    if owner != 0 && owner != request {
        pmu::stop();
        ADMISSION.release(cpu.get());
    }
    if request == 0 || owner == request || !ADMISSION.claim(cpu.get(), request) {
        return;
    }
    // Claim first: a racing stop must retain this capture until the hardware
    // has stopped, including a stop between claim and programming the counter.
    pmu::start(PERIOD.load(Ordering::Relaxed));
    if ADMISSION.requested() != request {
        pmu::stop();
        ADMISSION.release(cpu.get());
    }
}

/// Info uses `cpu=u64::MAX`; otherwise read the first retained sample at/after
/// cursor. Reading is non-destructive and includes its sequence to expose gaps.
pub(crate) fn query(generation: u64, cpu: u64, cursor: u64, out: &mut [u8]) -> Result<usize> {
    CONTROL.with(|config| {
        if !READY.load(Ordering::Acquire) {
            return Err(HYPER_NATIVE_STATUS_NOT_SUPPORTED);
        }
        if cpu == u64::MAX {
            if generation != 0 || cursor != 0 || out.len() < INFO_SIZE {
                return Err(HYPER_NATIVE_STATUS_INVALID_ARGUMENT);
            }
            let armed = (0..crate::cpu::online_cpu_count()).fold(0, |mask, cpu| {
                mask | (u64::from(ADMISSION.owner(cpu) != 0) << cpu)
            });
            encode(
                &[
                    config.generation,
                    config.period,
                    config.deadline,
                    crate::cpu::online_cpu_count() as u64,
                    ADMISSION.requested(),
                    armed,
                    CAPACITY as u64,
                    DEPTH as u64,
                    ADMISSION.active(),
                    DROPPED
                        .iter()
                        .map(|count| count.load(Ordering::Relaxed))
                        .sum(),
                ],
                out,
            );
            out[80..INFO_SIZE].fill(0);
            let revision = concat!(
                env!("HYPER_BUILD_REVISION"),
                " ",
                env!("HYPER_BUILD_TIMESTAMP")
            )
            .as_bytes();
            out[80..80 + revision.len().min(64)]
                .copy_from_slice(&revision[..revision.len().min(64)]);
            return Ok(INFO_SIZE);
        }
        if generation == 0 || generation != config.generation {
            return Err(HYPER_NATIVE_STATUS_BAD_STATE);
        }
        let cpu = usize::try_from(cpu)
            .ok()
            .filter(|cpu| *cpu < crate::cpu::online_cpu_count())
            .and_then(CpuIndex::new)
            .ok_or(HYPER_NATIVE_STATUS_INVALID_ARGUMENT)?;
        if out.len() < SAMPLE_SIZE {
            return Err(HYPER_NATIVE_STATUS_BUFFER_TOO_SMALL);
        }
        BUFFERS[cpu].with(|buffer| {
            let sequence = cursor.max(buffer.count.saturating_sub(CAPACITY as u64));
            if sequence >= buffer.count {
                return Err(HYPER_NATIVE_STATUS_WOULD_BLOCK);
            }
            encode(&buffer.samples[sequence as usize % CAPACITY].0, out);
            Ok(SAMPLE_SIZE)
        })
    })
}
fn encode(words: &[u64], bytes: &mut [u8]) {
    for (word, dst) in words.iter().zip(bytes.chunks_exact_mut(8)) {
        dst.copy_from_slice(&word.to_le_bytes());
    }
}

// Called only for this CPU's discovered PMU IRQ, with local IRQs masked.
// Ordinary interrupts never enter this path or inspect profiling control state.
fn handle_interrupt(_: VirtualInterrupt, _: usize, frame: &InterruptFrame<'_>) -> HandlerResult {
    sample_overflow(frame);
    HandlerResult::Handled
}

fn sample_overflow(frame: &InterruptFrame<'_>) {
    let Some(cpu) = crate::cpu::current_index() else {
        return;
    };
    let owner = ADMISSION.owner(cpu.get());
    if owner == 0 || ADMISSION.requested() != owner {
        // A pending level interrupt may outlive the capture that generated it.
        pmu::stop();
        if owner != 0 {
            ADMISSION.release(cpu.get());
        }
        return;
    }
    if !pmu::overflowed() {
        return;
    }
    pmu::stop();
    let Some(context) = frame.registers() else {
        ADMISSION.close(owner);
        let _ = notify_control();
        return;
    };
    let Ok(now) = time::monotonic_nanoseconds() else {
        ADMISSION.close(owner);
        let _ = notify_control();
        return;
    };
    let period = PERIOD.load(Ordering::Relaxed);
    if BUFFERS[cpu]
        .try_with(|buffer| {
            let mut sample = Sample([0; 24]);
            sample.0[0] = buffer.count;
            sample.0[1] = now;
            sample.0[2] = cpu.get() as u64;
            sample.0[3] = match context.origin {
                hyper::hal::interrupt::InterruptOrigin::Host => 0,
                hyper::hal::interrupt::InterruptOrigin::Native { .. } => 1,
                hyper::hal::interrupt::InterruptOrigin::Guest => 2,
            };
            sample.0[6] = DROPPED[cpu].load(Ordering::Relaxed);
            sample.0[7] = period;
            if matches!(context.origin, hyper::hal::interrupt::InterruptOrigin::Host) {
                unwind(cpu, context, &mut sample);
            }
            buffer.samples[buffer.count as usize % CAPACITY] = sample;
            buffer.count = buffer.count.saturating_add(1);
        })
        .is_none()
    {
        DROPPED[cpu].fetch_add(1, Ordering::Relaxed);
    }
    if ADMISSION.requested() == owner {
        pmu::reload(period);
    } else {
        ADMISSION.release(cpu.get());
    }
}

fn unwind(cpu: CpuIndex, context: InterruptedRegisters, sample: &mut Sample) {
    let base = addr_of!(__image_start) as usize;
    let end = addr_of!(__rodata_start) as usize;
    let offset = |pc: usize| (base..end).contains(&pc).then(|| (pc - base) as u64);
    let Some(pc) = offset(context.pc) else {
        sample.0[5] = 1;
        return;
    };
    sample.0[8] = pc;
    sample.0[4] = 1;
    let task = crate::task::scheduler::crash_snapshot(cpu.get());
    let bounds = task
        .and_then(|task| task.stack)
        .filter(|(bottom, top)| *bottom <= context.sp && context.sp < *top)
        .or_else(|| crate::hal::exception::bootstrap_stack_bounds(context.sp as u64))
        .or_else(|| crate::mm::stack::exception_stack_bounds(cpu.get(), context.sp));
    let Some((bottom, top)) = bounds else {
        sample.0[5] = 1;
        return;
    };
    let mut fp = context.fp;
    for depth in 1..DEPTH {
        // SAFETY: The local CPU is interrupted with IRQs masked, so its current
        // pinned kernel stack cannot execute or retire. Bounds reject user and
        // guest frames; HAL validates alignment and the entire frame record.
        let record = unsafe {
            crate::hal::exception::CrashContext::previous_stack_frame(
                fp,
                bottom.max(context.sp),
                top,
            )
        };
        let (previous, link) = match record {
            Ok(Some(frame)) => frame,
            Ok(None) => return,
            Err(_) => {
                sample.0[5] = 1;
                return;
            }
        };
        let Some(pc) = link.checked_sub(4).and_then(offset) else {
            return;
        };
        sample.0[8 + depth] = pc;
        sample.0[4] += 1;
        if previous <= fp {
            return;
        }
        fp = previous;
    }
    sample.0[5] = 1;
}
