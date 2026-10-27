// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! AMD Secure Virtual Machine (SVM) backend with nested paging.

pub(crate) mod registers;

use core::arch::asm;
use core::cell::UnsafeCell;
use core::ptr::{read_volatile, write_volatile};

use hyper::sync::atomic::{AtomicBool, AtomicU64, Ordering};
#[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
use hyper_vm_models::x86::svm::SvmFeatures;
use {
    hyper_vm_models::exit::GuestMemoryFault, hyper_vm_models::exit::GuestPhysicalAddress,
    hyper_vm_models::exit::MemoryAccess, hyper_vm_models::exit::MemoryFaultAction,
};
use {
    hyper_vm_models::x86::CpuidResult, hyper_vm_models::x86::GuestMsr,
    hyper_vm_models::x86::hypervisor_cpuid, hyper_vm_models::x86::sanitize_cpuid,
};
use {
    hyper_vm_models::x86::exit::PendingInterruptAction, hyper_vm_models::x86::exit::PortIoExit,
    hyper_vm_models::x86::exit::PortIoOperation, hyper_vm_models::x86::exit::PortIoWidth,
};
use {
    hyper_vm_models::x86::svm::IoDirection, hyper_vm_models::x86::svm::IoExit,
    hyper_vm_models::x86::svm::NptAccess, hyper_vm_models::x86::svm::NptViolation,
};

use crate::arch::x86_64::context::VcpuContext;
use crate::arch::x86_64::vm::svm::registers::*;
use {
    crate::arch::x86_64::vm::admission::PortIoCompletion,
    crate::arch::x86_64::vm::admission::PortIoCompletionError,
    crate::arch::x86_64::vm::admission::complete_port_io,
};

const MAX_CPUS: usize = hyper::config::MAX_CPUS as usize;
const PAGE_SIZE: usize = 4096;

const GUEST_BOOT_CR3: u64 = 0x70000;
const GUEST_GDT: u64 = 0x50000;
const GUEST_TSS: u64 = 0x51000;
const RESET_PAT: u64 = 0x0007_0406_0007_0406;

#[repr(C, align(4096))]
struct Page([u8; PAGE_SIZE]);

struct Pages(UnsafeCell<[Page; MAX_CPUS]>);
// SAFETY: Each array element is accessed only by its matching CPU with local
// interrupts masked. SVM hardware accesses that same element only during that
// CPU's serialized VMRUN transition; no Rust reference is retained across a
// transition that could conflict with hardware access.
unsafe impl Sync for Pages {}

#[repr(C, align(4096))]
struct IoPermissionMap([u8; PAGE_SIZE * 3]);

#[repr(C, align(4096))]
struct MsrPermissionMap([u8; PAGE_SIZE * 2]);

static VMCBS: Pages = Pages(UnsafeCell::new([const { Page([0; PAGE_SIZE]) }; MAX_CPUS]));
static HOST_SAVE_AREAS: Pages = Pages(UnsafeCell::new([const { Page([0; PAGE_SIZE]) }; MAX_CPUS]));
static IOPM: IoPermissionMap = IoPermissionMap([0xff; PAGE_SIZE * 3]);
static MSRPM: MsrPermissionMap = MsrPermissionMap([0xff; PAGE_SIZE * 2]);
static SVM_ACTIVE: [AtomicBool; MAX_CPUS] = [const { AtomicBool::new(false) }; MAX_CPUS];
static ACTIVE_NPT_ROOT: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(0) }; MAX_CPUS];
static TLB_PENDING: [AtomicBool; MAX_CPUS] = [const { AtomicBool::new(false) }; MAX_CPUS];
static TIMER_PENDING: [AtomicBool; MAX_CPUS] = [const { AtomicBool::new(false) }; MAX_CPUS];
static ACCEPTING_HOST_INTERRUPT: [AtomicBool; MAX_CPUS] =
    [const { AtomicBool::new(false) }; MAX_CPUS];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Error {
    InterruptsEnabled,
    InvalidAddress,
    InvalidCpu,
}

unsafe extern "C" {
    fn x86_64_svm_run(context: *mut VcpuContext, vmcb: u64, host_save: u64) -> !;
}

#[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
pub(crate) fn validate() -> Result<(), crate::arch::x86_64::vm::admission::ValidationError> {
    if read_msr(MSR_VM_CR) & VM_CR_SVM_DISABLE != 0 {
        return Err(crate::arch::x86_64::vm::admission::ValidationError::HardwareUnavailable);
    }
    let leaf = core::arch::x86_64::__cpuid(0x8000_000a);
    let features = SvmFeatures::decode(leaf.eax, leaf.ebx, leaf.edx);
    if !features.nested_paging || features.asids <= 1 {
        return Err(
            crate::arch::x86_64::vm::admission::ValidationError::SecondLevelPagingUnavailable,
        );
    }
    if !features.next_rip {
        return Err(crate::arch::x86_64::vm::admission::ValidationError::MissingNextRip);
    }
    Ok(())
}

pub(crate) unsafe fn enter(context: *mut VcpuContext) -> ! {
    if crate::arch::x86_64::local_irq_enabled() {
        crate::arch::x86_64::vm::fail_stop(
            "SVM guest entry with local IRQs enabled",
            Error::InterruptsEnabled,
        );
    }
    let cpu = crate::arch::x86_64::current_cpu_index();
    // SAFETY: The caller guarantees a valid exclusive context. The temporary
    // borrow ends before VMRUN and exit access through the raw pointer.
    let prepared = unsafe { prepare(cpu, &*context) };
    let (vmcb, host_save) = match prepared {
        Ok(addresses) => addresses,
        Err(error) => crate::arch::x86_64::vm::fail_stop("SVM guest entry preparation", error),
    };
    // SAFETY: No Rust reference to the context remains live across VMRUN.
    unsafe { x86_64_svm_run(context, vmcb, host_save) }
}

pub(crate) fn activate_npt(root: u64) {
    let cpu = crate::arch::x86_64::current_cpu_index();
    let Some(slot) = ACTIVE_NPT_ROOT.get(cpu) else {
        crate::arch::x86_64::vm::fail_stop("SVM NPT CPU lookup", Error::InvalidCpu);
    };
    slot.store(root, Ordering::Release);
    // The selected ASID is reused across VM address spaces. Defer the flush
    // until the VMCB is prepared so activation is valid both before and after
    // this CPU enters SVM operation.
    tlb_pending().store(true, Ordering::Release);
}

pub(crate) fn invalidate_npt() {
    tlb_pending().store(true, Ordering::Release);
}

fn prepare(cpu: usize, context: &VcpuContext) -> Result<(u64, u64), Error> {
    if cpu >= MAX_CPUS {
        return Err(Error::InvalidCpu);
    }
    enable(cpu)?;
    // SAFETY: Guest entry is serialized per CPU; this CPU exclusively owns its
    // page-aligned VMCB until the non-returning VMRUN transition.
    let vmcb = unsafe { &mut (*VMCBS.0.get())[cpu] };
    vmcb.0.fill(0);
    write_control_area(vmcb)?;
    write_guest_state(vmcb, context);
    let vmcb_pa = kernel_physical(vmcb.0.as_ptr() as usize)?;
    // SAFETY: Host-save pages are immutable after per-CPU initialization and
    // this in-bounds slot remains pinned for the kernel lifetime.
    let host = unsafe { &(*HOST_SAVE_AREAS.0.get())[cpu] };
    let host_pa = kernel_physical(host.0.as_ptr() as usize)?;
    Ok((vmcb_pa, host_pa))
}

fn enable(cpu: usize) -> Result<(), Error> {
    if SVM_ACTIVE[cpu].load(Ordering::Acquire) {
        return Ok(());
    }
    // SAFETY: Initialization is serialized by the per-CPU SVM_ACTIVE state;
    // this CPU exclusively initializes its pinned host-save page.
    let host = unsafe { &mut (*HOST_SAVE_AREAS.0.get())[cpu] };
    host.0.fill(0);
    let physical = kernel_physical(host.0.as_ptr() as usize)?;
    write_msr(
        GuestMsr::Efer.index(),
        read_msr(GuestMsr::Efer.index()) | EFER_SVME,
    );
    write_msr(MSR_VM_HSAVE_PA, physical);
    SVM_ACTIVE[cpu].store(true, Ordering::Release);
    Ok(())
}

fn write_control_area(vmcb: &mut Page) -> Result<(), Error> {
    let word3 = INTERCEPT_INTR
        | INTERCEPT_CPUID
        | INTERCEPT_HLT
        | INTERCEPT_IO
        | INTERCEPT_MSR
        | INTERCEPT_SHUTDOWN;
    vmcb.write_u32(VMCB_INTERCEPT_WORD3, word3);
    vmcb.write_u32(VMCB_INTERCEPT_WORD4, INTERCEPT_SVM_INSTRUCTIONS);
    let iopm = &IOPM;
    let msrpm = &MSRPM;
    vmcb.write_u64(VMCB_IOPM_BASE, kernel_physical(iopm.0.as_ptr() as usize)?);
    vmcb.write_u64(VMCB_MSRPM_BASE, kernel_physical(msrpm.0.as_ptr() as usize)?);
    vmcb.write_u32(VMCB_ASID, 1);
    vmcb.write_u8(
        VMCB_TLB_CONTROL,
        if tlb_pending().swap(false, Ordering::AcqRel) {
            1
        } else {
            0
        },
    );
    vmcb.write_u32(VMCB_INT_CONTROL, V_INTR_MASKING);
    vmcb.write_u64(VMCB_NESTED_CONTROL, 1);
    let root = active_npt_root().ok_or(Error::InvalidAddress)?;
    vmcb.write_u64(VMCB_NESTED_CR3, root);
    Ok(())
}

fn write_guest_state(vmcb: &mut Page, context: &VcpuContext) {
    vmcb.write_segment(SAVE_ES, 0x10, 0xc93, u32::MAX, 0);
    vmcb.write_segment(SAVE_CS, 0x08, 0xa9b, u32::MAX, 0);
    vmcb.write_segment(SAVE_SS, 0x10, 0xc93, u32::MAX, 0);
    vmcb.write_segment(SAVE_DS, 0x10, 0xc93, u32::MAX, 0);
    vmcb.write_segment(SAVE_FS, 0, 0xc93, u32::MAX, 0);
    vmcb.write_segment(SAVE_GS, 0, 0xc93, u32::MAX, 0);
    vmcb.write_segment(SAVE_GDTR, 0, 0, 39, GUEST_GDT);
    vmcb.write_segment(SAVE_LDTR, 0, 0, 0, 0);
    vmcb.write_segment(SAVE_IDTR, 0, 0, 0xffff, 0);
    vmcb.write_segment(SAVE_TR, 0x18, 0x08b, 0x67, GUEST_TSS);
    vmcb.write_u8(SAVE_CPL, 0);
    // VMRUN requires EFER.SVME in the guest save area even though SVM is not
    // advertised to the guest and all SVM instructions are intercepted.
    vmcb.write_u64(SAVE_EFER, EFER_SVME | (1 << 8) | (1 << 10) | (1 << 11));
    vmcb.write_u64(SAVE_CR4, 0x620);
    vmcb.write_u64(SAVE_CR3, GUEST_BOOT_CR3);
    vmcb.write_u64(SAVE_CR0, 0x8001_0033);
    vmcb.write_u64(SAVE_DR7, 0x400);
    vmcb.write_u64(SAVE_DR6, 0xffff_0ff0);
    vmcb.write_u64(SAVE_RFLAGS, context.flags | 2);
    vmcb.write_u64(SAVE_RIP, context.instruction_pointer);
    vmcb.write_u64(SAVE_RSP, context.general[4]);
    vmcb.write_u64(SAVE_RAX, context.general[0]);
    write_guest_msrs(vmcb, context);
}

fn write_guest_msrs(vmcb: &mut Page, context: &VcpuContext) {
    vmcb.write_u64(SAVE_STAR, context.msrs.star);
    vmcb.write_u64(SAVE_LSTAR, context.msrs.lstar);
    vmcb.write_u64(SAVE_CSTAR, context.msrs.cstar);
    vmcb.write_u64(SAVE_SFMASK, context.msrs.sfmask);
    vmcb.write_u64(SAVE_KERNEL_GS_BASE, context.msrs.kernel_gs_base);
    vmcb.write_u64(SAVE_SYSENTER_CS, 0);
    vmcb.write_u64(SAVE_SYSENTER_ESP, 0);
    vmcb.write_u64(SAVE_SYSENTER_EIP, 0);
    vmcb.write_u64(SAVE_PAT, RESET_PAT);
}

#[unsafe(no_mangle)]
unsafe extern "C" fn x86_64_svm_exit_dispatch(context: *mut VcpuContext) {
    if context.is_null() || !context.is_aligned() {
        crate::arch::x86_64::vm::fail_stop("invalid SVM exit context", Error::InvalidAddress);
    }
    let vmcb = active_vmcb_pointer();
    // SAFETY: Assembly passes the pinned context which exclusively owns this
    // VMRUN loop, and the active VMCB belongs exclusively to this CPU. Both
    // borrows end before any registered VM-exit service may reconstruct a
    // borrow of the containing VcpuExecution.
    unsafe { synchronize_context(&*vmcb, &mut *context) };
    // SAFETY: VM exit completed before this CPU-local VMCB read.
    let exit_code = unsafe { (&*vmcb).read_u64(VMCB_EXIT_CODE) };
    match exit_code {
        EXIT_INTR => handle_external_interrupt(),
        EXIT_CPUID => {
            // SAFETY: This backend-local branch invokes no entry service and
            // exclusively owns the pinned context until the borrow ends.
            let context = unsafe { &mut *context };
            emulate_cpuid(context);
            // SAFETY: This backend-local branch retains exclusive VMCB
            // ownership and invokes no registered service.
            let vmcb = unsafe { &mut *vmcb };
            vmcb.write_u64(SAVE_RAX, context.general[0]);
            advance_rip(vmcb);
        }
        EXIT_HLT => {
            // SAFETY: The active VMCB remains CPU-local between exits.
            unsafe { advance_rip(&mut *vmcb) }
        }
        EXIT_IO => {
            emulate_io(vmcb, context);
            // SAFETY: Port-I/O policy returned without retaining backend state.
            unsafe { advance_rip(&mut *vmcb) }
        }
        EXIT_MSR => {
            // SAFETY: MSR emulation is architecture-local and the borrow does
            // not cross a registered entry-service call.
            unsafe { emulate_msr(&mut *vmcb, &mut *context) }
        }
        EXIT_NPF => handle_npf(vmcb),
        EXIT_SHUTDOWN => crate::arch::x86_64::vm::fail_stop("SVM guest shutdown", exit_code),
        u64::MAX => {
            // SAFETY: Diagnostic reads observe this CPU's stopped VMCB.
            crate::arch::x86_64::vm::fail_stop("invalid SVM guest state", unsafe {
                dump_exit(&*vmcb)
            })
        }
        _ => {
            // SAFETY: Diagnostic reads observe this CPU's stopped VMCB.
            crate::arch::x86_64::vm::fail_stop("unhandled SVM exit", unsafe { dump_exit(&*vmcb) })
        }
    }
    prepare_guest_interrupt(vmcb);
    if tlb_pending().swap(false, Ordering::AcqRel) {
        // Full flush is supported without AMD's optional FlushByAsid feature.
        // HypeR currently reuses ASID 1, so the wider operation is required on
        // processors which do not advertise that extension.
        // SAFETY: No registered service is active and this CPU exclusively
        // owns its stopped VMCB.
        unsafe { (&mut *vmcb).write_u8(VMCB_TLB_CONTROL, 1) };
    }
    // SAFETY: The stopped VMCB remains exclusively owned until VMRUN.
    unsafe { (&mut *vmcb).write_u32(VMCB_CLEAN_BITS, 0) };
}

fn synchronize_context(vmcb: &Page, context: &mut VcpuContext) {
    context.general[0] = vmcb.read_u64(SAVE_RAX);
    context.general[4] = vmcb.read_u64(SAVE_RSP);
    context.instruction_pointer = vmcb.read_u64(SAVE_RIP);
    context.flags = vmcb.read_u64(SAVE_RFLAGS);
    context.msrs.star = vmcb.read_u64(SAVE_STAR);
    context.msrs.lstar = vmcb.read_u64(SAVE_LSTAR);
    context.msrs.cstar = vmcb.read_u64(SAVE_CSTAR);
    context.msrs.sfmask = vmcb.read_u64(SAVE_SFMASK);
    context.msrs.kernel_gs_base = vmcb.read_u64(SAVE_KERNEL_GS_BASE);
}

fn emulate_cpuid(context: &mut VcpuContext) {
    let leaf = context.general[0] as u32;
    let subleaf = context.general[2] as u32;
    let value = if leaf == 0x4000_0000 {
        hypervisor_cpuid()
    } else if (0x4000_0001..0x5000_0000).contains(&leaf) {
        CpuidResult::ZERO
    } else {
        let host = core::arch::x86_64::__cpuid_count(leaf, subleaf);
        sanitize_cpuid(
            leaf,
            subleaf,
            CpuidResult {
                eax: host.eax,
                ebx: host.ebx,
                ecx: host.ecx,
                edx: host.edx,
            },
        )
    };
    context.general[0] = u64::from(value.eax);
    context.general[1] = u64::from(value.ebx);
    context.general[2] = u64::from(value.ecx);
    context.general[3] = u64::from(value.edx);
}

fn emulate_io(vmcb: *mut Page, context: *mut VcpuContext) {
    // SAFETY: The caller owns the stopped CPU-local VMCB. This temporary
    // reference ends before the policy callback below.
    let info = unsafe { (&*vmcb).read_u64(VMCB_EXIT_INFO1) };
    let Some(exit) = IoExit::decode(info) else {
        crate::arch::x86_64::vm::fail_stop("invalid SVM I/O access", info);
    };
    if exit.string || exit.repeat {
        crate::arch::x86_64::vm::fail_stop("unsupported SVM string I/O", info);
    }
    // SAFETY: The VMRUN loop supplies its pinned context. Copy the only input
    // needed for policy before entering it, without retaining a Rust borrow.
    let accumulator = unsafe { core::ptr::addr_of!((*context).general[0]).read() };
    let Some(width) = PortIoWidth::from_bytes(exit.size) else {
        crate::arch::x86_64::vm::fail_stop("invalid SVM I/O access size", exit.size)
    };
    let operation = match exit.direction {
        IoDirection::Input => PortIoOperation::Input,
        IoDirection::Output => PortIoOperation::Output(accumulator as u32),
    };
    let event = PortIoExit::new(exit.port, width, operation);
    let action = crate::arch::vm::dispatch_port_io(event);
    match complete_port_io(accumulator, event, action) {
        Ok(PortIoCompletion::Input(merged)) => {
            // SAFETY: The policy callback returned and retained no
            // frame/context borrow. Reborrow the pinned context only to apply
            // its owned result.
            unsafe { core::ptr::addr_of_mut!((*context).general[0]).write(merged) };
            // SAFETY: The service returned, so the CPU-local VMCB can be
            // borrowed exclusively for completion.
            unsafe { (&mut *vmcb).write_u64(SAVE_RAX, merged) };
        }
        Ok(PortIoCompletion::Output) => {}
        Err(PortIoCompletionError::PolicyStopped) => {
            crate::arch::x86_64::vm::fail_stop("SVM I/O policy stopped the guest", event)
        }
        Err(PortIoCompletionError::ActionMismatch | PortIoCompletionError::InvalidWidth) => {
            crate::arch::x86_64::vm::fail_stop("invalid SVM I/O policy completion", (event, action))
        }
    }
}

fn emulate_msr(vmcb: &mut Page, context: &mut VcpuContext) {
    let msr = context.general[2] as u32;
    if vmcb.read_u64(VMCB_EXIT_INFO1) == 0 {
        let Some(value) = read_guest_msr(vmcb, context, msr) else {
            inject_general_protection(vmcb);
            return;
        };
        context.general[0] = value as u32 as u64;
        context.general[3] = (value >> 32) as u32 as u64;
    } else {
        let value = (context.general[0] as u32 as u64) | ((context.general[3] as u32 as u64) << 32);
        if !write_guest_msr(vmcb, context, msr, value) {
            inject_general_protection(vmcb);
            return;
        }
    }
    vmcb.write_u64(SAVE_RAX, context.general[0]);
    advance_rip(vmcb);
}

fn read_guest_msr(vmcb: &Page, context: &VcpuContext, msr: u32) -> Option<u64> {
    Some(match GuestMsr::decode(msr)? {
        GuestMsr::SysenterCs => vmcb.read_u64(SAVE_SYSENTER_CS),
        GuestMsr::SysenterEsp => vmcb.read_u64(SAVE_SYSENTER_ESP),
        GuestMsr::SysenterEip => vmcb.read_u64(SAVE_SYSENTER_EIP),
        GuestMsr::Efer => vmcb.read_u64(SAVE_EFER) & !EFER_SVME,
        GuestMsr::FsBase => vmcb.read_u64(SAVE_FS + 8),
        GuestMsr::GsBase => vmcb.read_u64(SAVE_GS + 8),
        GuestMsr::ApicBase => 0,
        GuestMsr::TimestampCounter => read_msr(GuestMsr::TimestampCounter.index()),
        GuestMsr::Pat => vmcb.read_u64(SAVE_PAT),
        GuestMsr::Star => context.msrs.star,
        GuestMsr::Lstar => context.msrs.lstar,
        GuestMsr::Cstar => context.msrs.cstar,
        GuestMsr::Sfmask => context.msrs.sfmask,
        GuestMsr::KernelGsBase => context.msrs.kernel_gs_base,
        GuestMsr::TscAux => context.msrs.tsc_aux,
    })
}

fn write_guest_msr(vmcb: &mut Page, context: &mut VcpuContext, msr: u32, value: u64) -> bool {
    let Some(msr) = GuestMsr::decode(msr) else {
        return false;
    };
    match msr {
        GuestMsr::SysenterCs => vmcb.write_u64(SAVE_SYSENTER_CS, value),
        GuestMsr::SysenterEsp => vmcb.write_u64(SAVE_SYSENTER_ESP, value),
        GuestMsr::SysenterEip => vmcb.write_u64(SAVE_SYSENTER_EIP, value),
        // SVME is a VMRUN implementation requirement, not part of the
        // guest-visible CPU contract (CPUID does not advertise SVM).
        GuestMsr::Efer => vmcb.write_u64(SAVE_EFER, value | EFER_SVME),
        GuestMsr::FsBase => vmcb.write_u64(SAVE_FS + 8, value),
        GuestMsr::GsBase => vmcb.write_u64(SAVE_GS + 8, value),
        GuestMsr::Pat => vmcb.write_u64(SAVE_PAT, value),
        GuestMsr::Star => {
            context.msrs.star = value;
            vmcb.write_u64(SAVE_STAR, value);
        }
        GuestMsr::Lstar => {
            context.msrs.lstar = value;
            vmcb.write_u64(SAVE_LSTAR, value);
        }
        GuestMsr::Cstar => {
            context.msrs.cstar = value;
            vmcb.write_u64(SAVE_CSTAR, value);
        }
        GuestMsr::Sfmask => {
            context.msrs.sfmask = value;
            vmcb.write_u64(SAVE_SFMASK, value);
        }
        GuestMsr::KernelGsBase => {
            context.msrs.kernel_gs_base = value;
            vmcb.write_u64(SAVE_KERNEL_GS_BASE, value);
        }
        GuestMsr::TscAux => context.msrs.tsc_aux = value,
        GuestMsr::TimestampCounter | GuestMsr::ApicBase => return false,
    }
    true
}

fn handle_npf(vmcb: *const Page) {
    // SAFETY: VM exit completed and the caller owns this CPU-local VMCB. Copy
    // all policy inputs before the registered service is invoked.
    let (info, address) = unsafe {
        (
            (&*vmcb).read_u64(VMCB_EXIT_INFO1),
            (&*vmcb).read_u64(VMCB_EXIT_INFO2),
        )
    };
    let violation = NptViolation::decode(info);
    let access = match violation.access {
        NptAccess::Read => MemoryAccess::Read,
        NptAccess::Write => MemoryAccess::Write,
        NptAccess::Execute => MemoryAccess::Execute,
    };
    let fault = GuestMemoryFault::new(
        GuestPhysicalAddress::new(address),
        access,
        violation.during_page_walk,
    );
    match crate::arch::vm::dispatch_memory_fault(fault) {
        MemoryFaultAction::Retry => {}
        MemoryFaultAction::ForwardToDevice => {
            crate::arch::x86_64::vm::fail_stop("unhandled NPT device access", (address, info))
        }
        MemoryFaultAction::Stop => crate::arch::x86_64::vm::fail_stop(
            "NPT fault policy stopped the guest",
            (address, info),
        ),
    }
}

fn handle_external_interrupt() {
    // SVM reports that a physical interrupt is pending, not its vector. Let
    // the local APIC deliver it through the normal IDT, then mask IRQs again
    // before the next VMRUN.
    let accepting = accepting_host_interrupt();
    accepting.store(true, Ordering::Release);
    // SAFETY: This executes at CPL0 on an SVM-enabled CPU. STGI/STI admits one
    // host interrupt window; CLI/CLGI closes it before returning to VMRUN.
    unsafe { asm!("stgi", "sti", "nop", "cli", "clgi", options(nostack)) };
    accepting.store(false, Ordering::Release);
}

pub(crate) fn observe_host_interrupt(vector: u32) {
    if vector == crate::arch::x86_64::platform::TIMER_VECTOR
        && accepting_host_interrupt().load(Ordering::Acquire)
    {
        timer_pending().store(true, Ordering::Release);
    }
}

fn prepare_guest_interrupt(vmcb: *mut Page) {
    let timer_is_pending = timer_pending().load(Ordering::Acquire);
    let (vector, consumes_timer) = match crate::arch::vm::query_pending_interrupt(timer_is_pending)
    {
        PendingInterruptAction::None => return,
        PendingInterruptAction::Inject {
            vector,
            consumes_timer,
        } => (vector, consumes_timer),
        PendingInterruptAction::Stop => {
            crate::arch::x86_64::vm::fail_stop("x86 interrupt-routing policy stopped the guest", ())
        }
    };
    // SAFETY: Interrupt-routing policy returned without retaining backend
    // state, so the stopped CPU-local VMCB may be borrowed for completion.
    let vmcb = unsafe { &mut *vmcb };
    let mut control = vmcb.read_u32(VMCB_INT_CONTROL);
    control &= !((0xf << V_INTR_PRIORITY_SHIFT) | V_IRQ | V_IGNORE_TPR);
    control |=
        V_INTR_MASKING | V_IRQ | V_IGNORE_TPR | (u32::from(vector >> 4) << V_INTR_PRIORITY_SHIFT);
    vmcb.write_u32(VMCB_INT_VECTOR, u32::from(vector));
    vmcb.write_u32(VMCB_INT_CONTROL, control);
    if consumes_timer {
        timer_pending().store(false, Ordering::Release);
    }
}

fn inject_general_protection(vmcb: &mut Page) {
    vmcb.write_u32(VMCB_EVENT_ERROR, 0);
    vmcb.write_u32(
        VMCB_EVENT_INJECTION,
        EVENT_VALID | EVENT_ERROR_VALID | EVENT_EXCEPTION | EXCEPTION_GENERAL_PROTECTION,
    );
}

fn advance_rip(vmcb: &mut Page) {
    vmcb.write_u64(SAVE_RIP, vmcb.read_u64(VMCB_NEXT_RIP));
}

fn active_vmcb_pointer() -> *mut Page {
    let cpu = crate::arch::x86_64::current_cpu_index();
    if cpu >= MAX_CPUS {
        crate::arch::x86_64::vm::fail_stop("SVM VMCB CPU lookup", Error::InvalidCpu);
    }
    // `Page` elements are contiguous at the start of the backing array. A raw
    // pointer is returned so lookup itself does not forge a `'static` mutable
    // reference; the serialized exit boundary creates the only reference.
    VMCBS.0.get().cast::<Page>().wrapping_add(cpu)
}

fn active_npt_root() -> Option<u64> {
    ACTIVE_NPT_ROOT
        .get(crate::arch::x86_64::current_cpu_index())
        .map(|slot| slot.load(Ordering::Acquire))
        .filter(|root| *root != 0)
}

fn tlb_pending() -> &'static AtomicBool {
    match TLB_PENDING.get(crate::arch::x86_64::current_cpu_index()) {
        Some(pending) => pending,
        None => crate::arch::x86_64::vm::fail_stop("SVM TLB CPU lookup", Error::InvalidCpu),
    }
}

fn timer_pending() -> &'static AtomicBool {
    match TIMER_PENDING.get(crate::arch::x86_64::current_cpu_index()) {
        Some(pending) => pending,
        None => crate::arch::x86_64::vm::fail_stop("SVM timer CPU lookup", Error::InvalidCpu),
    }
}

fn accepting_host_interrupt() -> &'static AtomicBool {
    match ACCEPTING_HOST_INTERRUPT.get(crate::arch::x86_64::current_cpu_index()) {
        Some(accepting) => accepting,
        None => crate::arch::x86_64::vm::fail_stop("SVM IRQ CPU lookup", Error::InvalidCpu),
    }
}

fn dump_exit(vmcb: &Page) -> (u64, u64, u64, u64) {
    (
        vmcb.read_u64(VMCB_EXIT_CODE),
        vmcb.read_u64(VMCB_EXIT_INFO1),
        vmcb.read_u64(VMCB_EXIT_INFO2),
        vmcb.read_u64(SAVE_RIP),
    )
}

impl Page {
    fn read_u32(&self, offset: usize) -> u32 {
        // SAFETY: Callers use aligned architectural VMCB offsets contained in
        // this pinned 4-KiB page.
        unsafe { read_volatile(self.0.as_ptr().add(offset).cast::<u32>()) }
    }

    fn read_u64(&self, offset: usize) -> u64 {
        // SAFETY: Callers use aligned architectural VMCB offsets contained in
        // this pinned 4-KiB page.
        unsafe { read_volatile(self.0.as_ptr().add(offset).cast::<u64>()) }
    }

    fn write_u8(&mut self, offset: usize, value: u8) {
        // SAFETY: The exclusive page borrow and architectural offset identify
        // one initialized byte within this pinned VMCB.
        unsafe { write_volatile(self.0.as_mut_ptr().add(offset), value) };
    }

    fn write_u32(&mut self, offset: usize, value: u32) {
        // SAFETY: Callers provide aligned architectural offsets within this
        // exclusively borrowed pinned VMCB page.
        unsafe { write_volatile(self.0.as_mut_ptr().add(offset).cast::<u32>(), value) };
    }

    fn write_u64(&mut self, offset: usize, value: u64) {
        // SAFETY: Callers provide aligned architectural offsets within this
        // exclusively borrowed pinned VMCB page.
        unsafe { write_volatile(self.0.as_mut_ptr().add(offset).cast::<u64>(), value) };
    }

    fn write_segment(&mut self, offset: usize, selector: u16, attr: u16, limit: u32, base: u64) {
        // SAFETY: `offset` is an aligned architectural segment-state offset;
        // all four fields fit inside this exclusively borrowed VMCB page.
        unsafe {
            write_volatile(self.0.as_mut_ptr().add(offset).cast::<u16>(), selector);
            write_volatile(self.0.as_mut_ptr().add(offset + 2).cast::<u16>(), attr);
            write_volatile(self.0.as_mut_ptr().add(offset + 4).cast::<u32>(), limit);
            write_volatile(self.0.as_mut_ptr().add(offset + 8).cast::<u64>(), base);
        }
    }
}

fn kernel_physical(virtual_address: usize) -> Result<u64, Error> {
    crate::arch::x86_64::memory::kernel_image_physical_address(virtual_address)
        .ok_or(Error::InvalidAddress)
}

fn read_msr(msr: u32) -> u64 {
    let low: u32;
    let high: u32;
    // SAFETY: This kernel executes at CPL0 and callers request MSRs admitted by
    // the SVM initialization and guest-state policy.
    unsafe { asm!("rdmsr", in("ecx") msr, out("eax") low, out("edx") high, options(nostack)) };
    u64::from(low) | (u64::from(high) << 32)
}

fn write_msr(msr: u32, value: u64) {
    // SAFETY: This kernel executes at CPL0 and callers write only the admitted
    // SVM/host-state MSRs with architecture-valid values.
    unsafe {
        asm!(
            "wrmsr",
            in("ecx") msr,
            in("eax") value as u32,
            in("edx") (value >> 32) as u32,
            options(nostack)
        )
    };
}
