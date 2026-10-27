// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Intel VMX lifecycle, VMCS policy, and VM-exit mechanism.
//!
//! Per-CPU VMXON/VMCS/MSR storage remains here because its exclusive ownership
//! is coupled to entry and exit. Raw privileged instruction bindings live in
//! `instruction`; guest ABI and kernel exit disposition remain outside this
//! module.

pub(crate) mod instruction;

use core::arch::asm;
use core::cell::UnsafeCell;
use core::ptr::{copy_nonoverlapping, read_unaligned, write_volatile};

use hyper::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
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
    hyper_vm_models::x86::vmx::ControlCapability, hyper_vm_models::x86::vmx::EptAccess,
    hyper_vm_models::x86::vmx::EptViolation, hyper_vm_models::x86::vmx::IoDirection,
    hyper_vm_models::x86::vmx::IoExit, hyper_vm_models::x86::vmx::VmxBasic,
};

use {crate::arch::x86_64::context::VcpuContext, crate::arch::x86_64::context::VcpuMsrState};
use {
    crate::arch::x86_64::vm::admission::PortIoCompletion,
    crate::arch::x86_64::vm::admission::PortIoCompletionError,
    crate::arch::x86_64::vm::admission::complete_port_io,
};

use {
    crate::arch::x86_64::vm::vmx::instruction::invept_single_context,
    crate::arch::x86_64::vm::vmx::instruction::read_cr0,
    crate::arch::x86_64::vm::vmx::instruction::read_cr3,
    crate::arch::x86_64::vm::vmx::instruction::read_cr4,
    crate::arch::x86_64::vm::vmx::instruction::read_cs,
    crate::arch::x86_64::vm::vmx::instruction::read_ds,
    crate::arch::x86_64::vm::vmx::instruction::read_es,
    crate::arch::x86_64::vm::vmx::instruction::read_fs,
    crate::arch::x86_64::vm::vmx::instruction::read_gs,
    crate::arch::x86_64::vm::vmx::instruction::read_msr,
    crate::arch::x86_64::vm::vmx::instruction::read_rsp,
    crate::arch::x86_64::vm::vmx::instruction::read_ss,
    crate::arch::x86_64::vm::vmx::instruction::read_tr,
    crate::arch::x86_64::vm::vmx::instruction::vmclear,
    crate::arch::x86_64::vm::vmx::instruction::vmptrld,
    crate::arch::x86_64::vm::vmx::instruction::vmread,
    crate::arch::x86_64::vm::vmx::instruction::vmwrite,
    crate::arch::x86_64::vm::vmx::instruction::vmxon,
    crate::arch::x86_64::vm::vmx::instruction::write_cr0,
    crate::arch::x86_64::vm::vmx::instruction::write_cr4,
    crate::arch::x86_64::vm::vmx::instruction::write_msr,
};

const MAX_CPUS: usize = hyper::config::MAX_CPUS as usize;
const PAGE_SIZE: usize = 4096;

const IA32_FEATURE_CONTROL: u32 = 0x3a;
const IA32_VMX_BASIC: u32 = 0x480;
const IA32_VMX_PINBASED_CTLS: u32 = 0x481;
const IA32_VMX_PROCBASED_CTLS: u32 = 0x482;
const IA32_VMX_EXIT_CTLS: u32 = 0x483;
const IA32_VMX_ENTRY_CTLS: u32 = 0x484;
const IA32_VMX_CR0_FIXED0: u32 = 0x486;
const IA32_VMX_CR0_FIXED1: u32 = 0x487;
const IA32_VMX_CR4_FIXED0: u32 = 0x488;
const IA32_VMX_CR4_FIXED1: u32 = 0x489;
const IA32_VMX_PROCBASED_CTLS2: u32 = 0x48b;
#[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
const IA32_VMX_EPT_VPID_CAP: u32 = 0x48c;
const IA32_VMX_TRUE_PINBASED_CTLS: u32 = 0x48d;
const IA32_VMX_TRUE_PROCBASED_CTLS: u32 = 0x48e;
const IA32_VMX_TRUE_EXIT_CTLS: u32 = 0x48f;
const IA32_VMX_TRUE_ENTRY_CTLS: u32 = 0x490;

const VMCS_CTRL_PIN: u64 = 0x4000;
const VMCS_CTRL_PRIMARY: u64 = 0x4002;
const VMCS_CTRL_EXCEPTION_BITMAP: u64 = 0x4004;
const VMCS_CTRL_PF_ERROR_MASK: u64 = 0x4006;
const VMCS_CTRL_PF_ERROR_MATCH: u64 = 0x4008;
const VMCS_CTRL_CR3_TARGET_COUNT: u64 = 0x400a;
const VMCS_CTRL_EXIT: u64 = 0x400c;
const VMCS_CTRL_EXIT_MSR_STORE_COUNT: u64 = 0x400e;
const VMCS_CTRL_EXIT_MSR_LOAD_COUNT: u64 = 0x4010;
const VMCS_CTRL_ENTRY: u64 = 0x4012;
const VMCS_CTRL_ENTRY_MSR_LOAD_COUNT: u64 = 0x4014;
const VMCS_CTRL_ENTRY_INTERRUPT: u64 = 0x4016;
const VMCS_CTRL_ENTRY_EXCEPTION_ERROR: u64 = 0x4018;
const VMCS_CTRL_SECONDARY: u64 = 0x401e;
const VMCS_EPT_POINTER: u64 = 0x201a;
const VMCS_EXIT_MSR_STORE_ADDRESS: u64 = 0x2006;
const VMCS_EXIT_MSR_LOAD_ADDRESS: u64 = 0x2008;
const VMCS_ENTRY_MSR_LOAD_ADDRESS: u64 = 0x200a;
const VMCS_LINK_POINTER: u64 = 0x2800;
const VMCS_GUEST_PAT: u64 = 0x2804;
const VMCS_GUEST_EFER: u64 = 0x2806;
const VMCS_HOST_PAT: u64 = 0x2c00;
const VMCS_HOST_EFER: u64 = 0x2c02;

const VMCS_GUEST_ES_SELECTOR: u64 = 0x0800;
const VMCS_GUEST_CS_SELECTOR: u64 = 0x0802;
const VMCS_GUEST_SS_SELECTOR: u64 = 0x0804;
const VMCS_GUEST_DS_SELECTOR: u64 = 0x0806;
const VMCS_GUEST_FS_SELECTOR: u64 = 0x0808;
const VMCS_GUEST_GS_SELECTOR: u64 = 0x080a;
const VMCS_GUEST_LDTR_SELECTOR: u64 = 0x080c;
const VMCS_GUEST_TR_SELECTOR: u64 = 0x080e;
const VMCS_HOST_ES_SELECTOR: u64 = 0x0c00;
const VMCS_HOST_CS_SELECTOR: u64 = 0x0c02;
const VMCS_HOST_SS_SELECTOR: u64 = 0x0c04;
const VMCS_HOST_DS_SELECTOR: u64 = 0x0c06;
const VMCS_HOST_FS_SELECTOR: u64 = 0x0c08;
const VMCS_HOST_GS_SELECTOR: u64 = 0x0c0a;
const VMCS_HOST_TR_SELECTOR: u64 = 0x0c0c;

const VMCS_GUEST_ES_LIMIT: u64 = 0x4800;
const VMCS_GUEST_CS_LIMIT: u64 = 0x4802;
const VMCS_GUEST_SS_LIMIT: u64 = 0x4804;
const VMCS_GUEST_DS_LIMIT: u64 = 0x4806;
const VMCS_GUEST_FS_LIMIT: u64 = 0x4808;
const VMCS_GUEST_GS_LIMIT: u64 = 0x480a;
const VMCS_GUEST_LDTR_LIMIT: u64 = 0x480c;
const VMCS_GUEST_TR_LIMIT: u64 = 0x480e;
const VMCS_GUEST_GDTR_LIMIT: u64 = 0x4810;
const VMCS_GUEST_IDTR_LIMIT: u64 = 0x4812;
const VMCS_GUEST_ES_AR: u64 = 0x4814;
const VMCS_GUEST_CS_AR: u64 = 0x4816;
const VMCS_GUEST_SS_AR: u64 = 0x4818;
const VMCS_GUEST_DS_AR: u64 = 0x481a;
const VMCS_GUEST_FS_AR: u64 = 0x481c;
const VMCS_GUEST_GS_AR: u64 = 0x481e;
const VMCS_GUEST_LDTR_AR: u64 = 0x4820;
const VMCS_GUEST_TR_AR: u64 = 0x4822;
const VMCS_GUEST_INTERRUPTIBILITY: u64 = 0x4824;
const VMCS_GUEST_ACTIVITY: u64 = 0x4826;
const VMCS_GUEST_SYSENTER_CS: u64 = 0x482a;
const VMCS_HOST_SYSENTER_CS: u64 = 0x4c00;

const VMCS_CTRL_CR0_MASK: u64 = 0x6000;
const VMCS_CTRL_CR4_MASK: u64 = 0x6002;
const VMCS_CTRL_CR0_SHADOW: u64 = 0x6004;
const VMCS_CTRL_CR4_SHADOW: u64 = 0x6006;

const VMCS_GUEST_CR0: u64 = 0x6800;
const VMCS_GUEST_CR3: u64 = 0x6802;
const VMCS_GUEST_CR4: u64 = 0x6804;
const VMCS_GUEST_ES_BASE: u64 = 0x6806;
const VMCS_GUEST_CS_BASE: u64 = 0x6808;
const VMCS_GUEST_SS_BASE: u64 = 0x680a;
const VMCS_GUEST_DS_BASE: u64 = 0x680c;
const VMCS_GUEST_FS_BASE: u64 = 0x680e;
const VMCS_GUEST_GS_BASE: u64 = 0x6810;
const VMCS_GUEST_LDTR_BASE: u64 = 0x6812;
const VMCS_GUEST_TR_BASE: u64 = 0x6814;
const VMCS_GUEST_GDTR_BASE: u64 = 0x6816;
const VMCS_GUEST_IDTR_BASE: u64 = 0x6818;
const VMCS_GUEST_DR7: u64 = 0x681a;
const VMCS_GUEST_RSP: u64 = 0x681c;
const VMCS_GUEST_RIP: u64 = 0x681e;
const VMCS_GUEST_RFLAGS: u64 = 0x6820;
const VMCS_GUEST_PENDING_DEBUG: u64 = 0x6822;
const VMCS_GUEST_SYSENTER_ESP: u64 = 0x6824;
const VMCS_GUEST_SYSENTER_EIP: u64 = 0x6826;

const VMCS_HOST_CR0: u64 = 0x6c00;
const VMCS_HOST_CR3: u64 = 0x6c02;
const VMCS_HOST_CR4: u64 = 0x6c04;
const VMCS_HOST_FS_BASE: u64 = 0x6c06;
const VMCS_HOST_GS_BASE: u64 = 0x6c08;
const VMCS_HOST_TR_BASE: u64 = 0x6c0a;
const VMCS_HOST_GDTR_BASE: u64 = 0x6c0c;
const VMCS_HOST_IDTR_BASE: u64 = 0x6c0e;
const VMCS_HOST_SYSENTER_ESP: u64 = 0x6c10;
const VMCS_HOST_SYSENTER_EIP: u64 = 0x6c12;
const VMCS_HOST_RSP: u64 = 0x6c14;
const VMCS_HOST_RIP: u64 = 0x6c16;

const VMCS_EXIT_REASON: u64 = 0x4402;
const VMCS_EXIT_INTERRUPT_INFO: u64 = 0x4404;
const VMCS_EXIT_INSTRUCTION_LENGTH: u64 = 0x440c;
const VMCS_EXIT_QUALIFICATION: u64 = 0x6400;
const VMCS_GUEST_PHYSICAL_ADDRESS: u64 = 0x2400;
const VMCS_INSTRUCTION_ERROR: u64 = 0x4400;

const PRIMARY_INTERRUPT_WINDOW: u32 = 1 << 2;
const PRIMARY_HLT_EXITING: u32 = 1 << 7;
const PRIMARY_UNCONDITIONAL_IO: u32 = 1 << 24;
const PRIMARY_SECONDARY_CONTROLS: u32 = 1 << 31;
const PIN_EXTERNAL_INTERRUPT_EXITING: u32 = 1;
const SECONDARY_EPT: u32 = 1 << 1;
const EXIT_HOST_64_BIT: u32 = 1 << 9;
const EXIT_ACK_INTERRUPT: u32 = 1 << 15;
const EXIT_SAVE_PAT: u32 = 1 << 18;
const EXIT_LOAD_PAT: u32 = 1 << 19;
const EXIT_SAVE_EFER: u32 = 1 << 20;
const EXIT_LOAD_EFER: u32 = 1 << 21;
const ENTRY_GUEST_64_BIT: u32 = 1 << 9;
const ENTRY_LOAD_PAT: u32 = 1 << 14;
const ENTRY_LOAD_EFER: u32 = 1 << 15;

const EXIT_EXTERNAL_INTERRUPT: u32 = 1;
const EXIT_INTERRUPT_WINDOW: u32 = 7;
const EXIT_CPUID: u32 = 10;
const EXIT_HLT: u32 = 12;
const EXIT_IO: u32 = 30;
const EXIT_RDMSR: u32 = 31;
const EXIT_WRMSR: u32 = 32;
const EXIT_EPT_VIOLATION: u32 = 48;
const EXIT_EPT_MISCONFIGURATION: u32 = 49;

const INTERRUPTION_VALID: u64 = 1 << 31;
const INTERRUPTION_ERROR_CODE: u64 = 1 << 11;
const INTERRUPTION_HARDWARE_EXCEPTION: u64 = 3 << 8;
const EXCEPTION_GENERAL_PROTECTION: u64 = 13;

const GUEST_BOOT_CR3: u64 = 0x70000;
const GUEST_GDT: u64 = 0x50000;
const GUEST_TSS: u64 = 0x51000;
const RESET_PAT: u64 = 0x0007_0406_0007_0406;
const GUEST_MSR_COUNT: usize = 5;

#[repr(C, align(4096))]
struct VmxPage([u8; PAGE_SIZE]);
struct VmxPages(UnsafeCell<[VmxPage; MAX_CPUS]>);
// SAFETY: Each page is exclusively owned by its matching CPU while local
// interrupts are masked. VMX hardware access is serialized by VMXON/VM entry,
// and Rust never retains a reference across a conflicting hardware access.
unsafe impl Sync for VmxPages {}

#[derive(Clone, Copy)]
#[repr(C)]
struct MsrEntry {
    index: u32,
    reserved: u32,
    value: u64,
}

impl MsrEntry {
    const EMPTY: Self = Self {
        index: 0,
        reserved: 0,
        value: 0,
    };

    const fn new(index: u32, value: u64) -> Self {
        Self {
            index,
            reserved: 0,
            value,
        }
    }
}

#[repr(C, align(16))]
struct MsrList([MsrEntry; GUEST_MSR_COUNT]);

const _: () = {
    assert!(core::mem::size_of::<MsrEntry>() == 16);
    assert!(core::mem::align_of::<MsrList>() >= 16);
};

struct MsrLists(UnsafeCell<[MsrList; MAX_CPUS]>);

// SAFETY: Each list is accessed only by its matching CPU while local IRQs are
// masked, or by VMX hardware as part of that CPU's entry/exit transition.
unsafe impl Sync for MsrLists {}

static VMXON_PAGES: VmxPages = VmxPages(UnsafeCell::new(
    [const { VmxPage([0; PAGE_SIZE]) }; MAX_CPUS],
));
static VMCS_PAGES: VmxPages = VmxPages(UnsafeCell::new(
    [const { VmxPage([0; PAGE_SIZE]) }; MAX_CPUS],
));
static GUEST_MSR_LISTS: MsrLists = MsrLists(UnsafeCell::new(
    [const { MsrList([MsrEntry::EMPTY; GUEST_MSR_COUNT]) }; MAX_CPUS],
));
static HOST_MSR_LISTS: MsrLists = MsrLists(UnsafeCell::new(
    [const { MsrList([MsrEntry::EMPTY; GUEST_MSR_COUNT]) }; MAX_CPUS],
));
static VMX_ACTIVE: [AtomicBool; MAX_CPUS] = [const { AtomicBool::new(false) }; MAX_CPUS];
static ACTIVE_CONTEXT: [AtomicUsize; MAX_CPUS] = [const { AtomicUsize::new(0) }; MAX_CPUS];
static ACTIVE_EPTP: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(0) }; MAX_CPUS];
static EPT_INVALIDATION_PENDING: [AtomicBool; MAX_CPUS] =
    [const { AtomicBool::new(false) }; MAX_CPUS];
static TIMER_PENDING: [AtomicBool; MAX_CPUS] = [const { AtomicBool::new(false) }; MAX_CPUS];

#[repr(C)]
pub struct ExitRegisters {
    r15: u64,
    r14: u64,
    r13: u64,
    r12: u64,
    r11: u64,
    r10: u64,
    r9: u64,
    r8: u64,
    rdi: u64,
    rsi: u64,
    rbp: u64,
    rdx: u64,
    rcx: u64,
    rbx: u64,
    rax: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    InvalidAddress,
    InvalidCpu,
    InvalidControl,
    InterruptsEnabled,
    UnsupportedVmxRegion,
    VmxInactive,
    VmxInstruction,
}

unsafe extern "C" {
    fn x86_64_vmlaunch(context: *const VcpuContext) -> !;
    fn x86_64_vmexit_entry();
}

pub unsafe fn enter(context: *mut VcpuContext) -> ! {
    if crate::arch::x86_64::local_irq_enabled() {
        crate::arch::x86_64::vm::fail_stop(
            "VMX guest entry with local IRQs enabled",
            Error::InterruptsEnabled,
        );
    }
    // SAFETY: The caller guarantees a valid exclusive context. The temporary
    // borrow ends before VMLAUNCH and VM-exit access through the raw pointer.
    let preparation = unsafe { prepare_vmcs(&*context) };
    if let Err(error) = preparation {
        crate::arch::x86_64::vm::fail_stop("VMX guest entry preparation", error);
    }
    let cpu = crate::arch::x86_64::current_cpu_index();
    let Some(slot) = ACTIVE_CONTEXT.get(cpu) else {
        crate::arch::x86_64::vm::fail_stop("VMX active-context publication", cpu);
    };
    slot.store(context.expose_provenance(), Ordering::Release);
    // VM exits continue on the scheduler-owned, guarded vCPU kernel stack.
    // Bias by eight bytes so the assembly register frame and FXSAVE area keep
    // the SysV call-site and hardware buffer alignment requirements.
    write_vmcs(VMCS_HOST_RSP, (read_rsp() & !15).wrapping_sub(8));
    // SAFETY: No Rust reference to the context remains live across VM entry.
    unsafe { x86_64_vmlaunch(context.cast_const()) }
}

#[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
pub(crate) fn validate() -> Result<(), crate::arch::x86_64::vm::admission::ValidationError> {
    let basic_features = core::arch::x86_64::__cpuid(1);
    if basic_features.ecx & (1 << 5) == 0 {
        return Err(crate::arch::x86_64::vm::admission::ValidationError::HardwareUnavailable);
    }
    if !VmxBasic::decode(read_msr(IA32_VMX_BASIC)).is_supported() {
        return Err(crate::arch::x86_64::vm::admission::ValidationError::HardwareUnavailable);
    }
    let primary = ControlCapability::decode(read_msr(control_msr(
        IA32_VMX_PROCBASED_CTLS,
        IA32_VMX_TRUE_PROCBASED_CTLS,
    )));
    if primary.may_be_one & PRIMARY_SECONDARY_CONTROLS == 0 {
        return Err(
            crate::arch::x86_64::vm::admission::ValidationError::SecondLevelPagingUnavailable,
        );
    }
    let secondary = read_msr(IA32_VMX_PROCBASED_CTLS2) >> 32;
    let ept = read_msr(IA32_VMX_EPT_VPID_CAP);
    let required_ept = (1 << 6) | (1 << 14) | (1 << 20) | (1 << 25);
    if secondary & u64::from(SECONDARY_EPT) == 0 || ept & required_ept != required_ept {
        return Err(
            crate::arch::x86_64::vm::admission::ValidationError::SecondLevelPagingUnavailable,
        );
    }
    Ok(())
}

fn prepare_vmcs(context: &VcpuContext) -> Result<(), Error> {
    let cpu = crate::arch::x86_64::current_cpu_index();
    if cpu >= MAX_CPUS {
        return Err(Error::InvalidCpu);
    }
    enable_vmx(cpu)?;
    // SAFETY: VM entry is serialized per CPU; this CPU exclusively prepares
    // its pinned, page-aligned VMCS region.
    let vmcs = unsafe { &mut (*VMCS_PAGES.0.get())[cpu] };
    vmcs.0.fill(0);
    let basic = VmxBasic::decode(read_msr(IA32_VMX_BASIC));
    // SAFETY: The exclusive page borrow provides an aligned initialized u32 at
    // the architectural VMCS revision offset zero.
    unsafe { write_volatile(vmcs.0.as_mut_ptr().cast::<u32>(), basic.revision) };
    let vmcs_pa = kernel_physical(vmcs.0.as_ptr() as usize)?;
    if !basic.accepts_region(vmcs_pa) {
        return Err(Error::InvalidAddress);
    }
    vmclear(vmcs_pa)?;
    vmptrld(vmcs_pa)?;

    let eptp = active_eptp().ok_or(Error::InvalidAddress)?;
    if EPT_INVALIDATION_PENDING[cpu].swap(false, Ordering::AcqRel) {
        // VMXON must precede INVEPT. Stage-2 activation can occur before this
        // CPU has entered VMX operation, so it records the required flush and
        // the first safe pre-entry point consumes it here.
        invept_single_context(eptp)?;
    }
    write_controls(eptp)?;
    write_guest_state(context)?;
    write_host_state()?;
    write_msr_lists(cpu, context)?;
    Ok(())
}

fn write_msr_lists(cpu: usize, context: &VcpuContext) -> Result<(), Error> {
    // SAFETY: `cpu` was validated against MAX_CPUS, VMX is stopped, and this
    // CPU exclusively owns both per-CPU lists until the following VM entry.
    let guest = unsafe { &mut (*GUEST_MSR_LISTS.0.get())[cpu] };
    guest.0 = msr_entries(context.msrs);
    // SAFETY: The same per-CPU ownership rule applies to the host restore list.
    let host = unsafe { &mut (*HOST_MSR_LISTS.0.get())[cpu] };
    host.0 = msr_entries(VcpuMsrState {
        star: read_msr(GuestMsr::Star.index()),
        lstar: read_msr(GuestMsr::Lstar.index()),
        cstar: read_msr(GuestMsr::Cstar.index()),
        sfmask: read_msr(GuestMsr::Sfmask.index()),
        kernel_gs_base: read_msr(GuestMsr::KernelGsBase.index()),
        tsc_aux: 0,
    });
    let guest_address = kernel_physical(guest.0.as_ptr() as usize)?;
    let host_address = kernel_physical(host.0.as_ptr() as usize)?;
    vmwrite(VMCS_EXIT_MSR_STORE_ADDRESS, guest_address)?;
    vmwrite(VMCS_EXIT_MSR_LOAD_ADDRESS, host_address)?;
    vmwrite(VMCS_ENTRY_MSR_LOAD_ADDRESS, guest_address)?;
    vmwrite(VMCS_CTRL_EXIT_MSR_STORE_COUNT, GUEST_MSR_COUNT as u64)?;
    vmwrite(VMCS_CTRL_EXIT_MSR_LOAD_COUNT, GUEST_MSR_COUNT as u64)?;
    vmwrite(VMCS_CTRL_ENTRY_MSR_LOAD_COUNT, GUEST_MSR_COUNT as u64)
}

const fn msr_entries(state: VcpuMsrState) -> [MsrEntry; GUEST_MSR_COUNT] {
    [
        MsrEntry::new(GuestMsr::Star.index(), state.star),
        MsrEntry::new(GuestMsr::Lstar.index(), state.lstar),
        MsrEntry::new(GuestMsr::Cstar.index(), state.cstar),
        MsrEntry::new(GuestMsr::Sfmask.index(), state.sfmask),
        MsrEntry::new(GuestMsr::KernelGsBase.index(), state.kernel_gs_base),
    ]
}

fn enable_vmx(cpu: usize) -> Result<(), Error> {
    if VMX_ACTIVE[cpu].load(Ordering::Acquire) {
        return Ok(());
    }
    let feature = read_msr(IA32_FEATURE_CONTROL);
    if feature & 1 == 0 {
        // SAFETY: IA32_FEATURE_CONTROL is unlocked, and this one-time write
        // enables VMX outside SMX while atomically locking the policy.
        unsafe { write_msr(IA32_FEATURE_CONTROL, feature | 1 | (1 << 2)) };
        if read_msr(IA32_FEATURE_CONTROL) & 5 != 5 {
            return Err(Error::InvalidControl);
        }
    } else if feature & (1 << 2) == 0 {
        return Err(Error::InvalidControl);
    }
    let mut cr0 = read_cr0();
    cr0 |= read_msr(IA32_VMX_CR0_FIXED0);
    cr0 &= read_msr(IA32_VMX_CR0_FIXED1);
    let mut cr4 = read_cr4() | (1 << 13);
    cr4 |= read_msr(IA32_VMX_CR4_FIXED0);
    cr4 &= read_msr(IA32_VMX_CR4_FIXED1);
    // SAFETY: Both values were derived from the current registers and Intel's
    // mandatory fixed masks; CR4 additionally admits VMXE.
    unsafe {
        write_cr0(cr0);
        write_cr4(cr4);
    }

    // SAFETY: VMX activation is serialized per CPU; this CPU exclusively
    // initializes its pinned, page-aligned VMXON region.
    let page = unsafe { &mut (*VMXON_PAGES.0.get())[cpu] };
    page.0.fill(0);
    let basic = VmxBasic::decode(read_msr(IA32_VMX_BASIC));
    if !basic.is_supported() {
        return Err(Error::UnsupportedVmxRegion);
    }
    // SAFETY: The exclusive page borrow provides an aligned initialized u32 at
    // the architectural VMXON revision offset zero.
    unsafe { write_volatile(page.0.as_mut_ptr().cast::<u32>(), basic.revision) };
    let physical = kernel_physical(page.0.as_ptr() as usize)?;
    if !basic.accepts_region(physical) {
        return Err(Error::InvalidAddress);
    }
    vmxon(physical)?;
    VMX_ACTIVE[cpu].store(true, Ordering::Release);
    Ok(())
}

fn write_controls(eptp: u64) -> Result<(), Error> {
    vmwrite(
        VMCS_CTRL_PIN,
        u64::from(adjust_control(
            PIN_EXTERNAL_INTERRUPT_EXITING,
            control_msr(IA32_VMX_PINBASED_CTLS, IA32_VMX_TRUE_PINBASED_CTLS),
        )?),
    )?;
    vmwrite(
        VMCS_CTRL_PRIMARY,
        u64::from(adjust_control(
            PRIMARY_HLT_EXITING | PRIMARY_UNCONDITIONAL_IO | PRIMARY_SECONDARY_CONTROLS,
            control_msr(IA32_VMX_PROCBASED_CTLS, IA32_VMX_TRUE_PROCBASED_CTLS),
        )?),
    )?;
    vmwrite(
        VMCS_CTRL_SECONDARY,
        u64::from(adjust_control(SECONDARY_EPT, IA32_VMX_PROCBASED_CTLS2)?),
    )?;
    vmwrite(
        VMCS_CTRL_EXIT,
        u64::from(adjust_control(
            EXIT_HOST_64_BIT
                | EXIT_ACK_INTERRUPT
                | EXIT_SAVE_PAT
                | EXIT_LOAD_PAT
                | EXIT_SAVE_EFER
                | EXIT_LOAD_EFER,
            control_msr(IA32_VMX_EXIT_CTLS, IA32_VMX_TRUE_EXIT_CTLS),
        )?),
    )?;
    vmwrite(
        VMCS_CTRL_ENTRY,
        u64::from(adjust_control(
            ENTRY_GUEST_64_BIT | ENTRY_LOAD_PAT | ENTRY_LOAD_EFER,
            control_msr(IA32_VMX_ENTRY_CTLS, IA32_VMX_TRUE_ENTRY_CTLS),
        )?),
    )?;
    vmwrite(VMCS_CTRL_EXCEPTION_BITMAP, 0)?;
    vmwrite(VMCS_CTRL_PF_ERROR_MASK, 0)?;
    vmwrite(VMCS_CTRL_PF_ERROR_MATCH, 0)?;
    vmwrite(VMCS_CTRL_CR3_TARGET_COUNT, 0)?;
    vmwrite(VMCS_CTRL_ENTRY_INTERRUPT, 0)?;
    vmwrite(VMCS_CTRL_ENTRY_EXCEPTION_ERROR, 0)?;
    vmwrite(VMCS_CTRL_CR0_MASK, 0)?;
    vmwrite(VMCS_CTRL_CR4_MASK, 0)?;
    vmwrite(VMCS_CTRL_CR0_SHADOW, 0x8001_0033)?;
    vmwrite(VMCS_CTRL_CR4_SHADOW, 0x620)?;
    vmwrite(VMCS_EPT_POINTER, eptp)?;
    Ok(())
}

fn write_guest_state(context: &VcpuContext) -> Result<(), Error> {
    for (field, value) in [
        (VMCS_GUEST_ES_SELECTOR, 0x10),
        (VMCS_GUEST_CS_SELECTOR, 0x08),
        (VMCS_GUEST_SS_SELECTOR, 0x10),
        (VMCS_GUEST_DS_SELECTOR, 0x10),
        (VMCS_GUEST_FS_SELECTOR, 0),
        (VMCS_GUEST_GS_SELECTOR, 0),
        (VMCS_GUEST_LDTR_SELECTOR, 0),
        (VMCS_GUEST_TR_SELECTOR, 0x18),
    ] {
        vmwrite(field, value)?;
    }
    for field in [
        VMCS_GUEST_ES_LIMIT,
        VMCS_GUEST_CS_LIMIT,
        VMCS_GUEST_SS_LIMIT,
        VMCS_GUEST_DS_LIMIT,
        VMCS_GUEST_FS_LIMIT,
        VMCS_GUEST_GS_LIMIT,
    ] {
        vmwrite(field, 0xffff_ffff)?;
    }
    vmwrite(VMCS_GUEST_LDTR_LIMIT, 0)?;
    vmwrite(VMCS_GUEST_TR_LIMIT, 0x67)?;
    vmwrite(VMCS_GUEST_GDTR_LIMIT, 39)?;
    vmwrite(VMCS_GUEST_IDTR_LIMIT, 0xffff)?;
    for field in [VMCS_GUEST_ES_AR, VMCS_GUEST_SS_AR, VMCS_GUEST_DS_AR] {
        vmwrite(field, 0xc093)?;
    }
    vmwrite(VMCS_GUEST_FS_AR, 1 << 16)?;
    vmwrite(VMCS_GUEST_GS_AR, 1 << 16)?;
    vmwrite(VMCS_GUEST_CS_AR, 0xa09b)?;
    vmwrite(VMCS_GUEST_LDTR_AR, 1 << 16)?;
    vmwrite(VMCS_GUEST_TR_AR, 0x008b)?;
    for field in [
        VMCS_GUEST_ES_BASE,
        VMCS_GUEST_CS_BASE,
        VMCS_GUEST_SS_BASE,
        VMCS_GUEST_DS_BASE,
        VMCS_GUEST_FS_BASE,
        VMCS_GUEST_GS_BASE,
        VMCS_GUEST_LDTR_BASE,
    ] {
        vmwrite(field, 0)?;
    }
    vmwrite(VMCS_GUEST_TR_BASE, GUEST_TSS)?;
    vmwrite(VMCS_GUEST_GDTR_BASE, GUEST_GDT)?;
    vmwrite(VMCS_GUEST_IDTR_BASE, 0)?;
    vmwrite(VMCS_GUEST_CR0, 0x8001_0033)?;
    vmwrite(VMCS_GUEST_CR3, GUEST_BOOT_CR3)?;
    vmwrite(VMCS_GUEST_CR4, 0x620)?;
    vmwrite(VMCS_GUEST_DR7, 0x400)?;
    vmwrite(VMCS_GUEST_RSP, context.general[4])?;
    vmwrite(VMCS_GUEST_RIP, context.instruction_pointer)?;
    vmwrite(VMCS_GUEST_RFLAGS, context.flags | 2)?;
    vmwrite(VMCS_GUEST_PENDING_DEBUG, 0)?;
    vmwrite(VMCS_GUEST_INTERRUPTIBILITY, 0)?;
    vmwrite(VMCS_GUEST_ACTIVITY, 0)?;
    vmwrite(VMCS_GUEST_SYSENTER_CS, 0)?;
    vmwrite(VMCS_GUEST_SYSENTER_ESP, 0)?;
    vmwrite(VMCS_GUEST_SYSENTER_EIP, 0)?;
    vmwrite(VMCS_GUEST_PAT, RESET_PAT)?;
    vmwrite(VMCS_GUEST_EFER, (1 << 8) | (1 << 10) | (1 << 11))?;
    vmwrite(VMCS_LINK_POINTER, u64::MAX)
}

fn write_host_state() -> Result<(), Error> {
    let mut gdtr = [0_u8; 10];
    let mut idtr = [0_u8; 10];
    // SAFETY: Both writable ten-byte buffers exactly match the SGDT/SIDT
    // memory operand format and remain live for these instructions.
    unsafe {
        asm!("sgdt [{}]", in(reg) gdtr.as_mut_ptr(), options(nostack));
        asm!("sidt [{}]", in(reg) idtr.as_mut_ptr(), options(nostack));
    }
    // SAFETY: SGDT initialized all ten bytes; bytes 2..10 contain the possibly
    // unaligned 64-bit descriptor-table base.
    let gdtr_base = unsafe { read_unaligned(gdtr.as_ptr().add(2).cast::<u64>()) };
    // SAFETY: SIDT initialized all ten bytes with the same packed layout.
    let idtr_base = unsafe { read_unaligned(idtr.as_ptr().add(2).cast::<u64>()) };
    let tr = read_tr();
    let tr_base = descriptor_base(gdtr_base, tr)?;
    for (field, value) in [
        (VMCS_HOST_ES_SELECTOR, u64::from(read_es() & !7)),
        (VMCS_HOST_CS_SELECTOR, u64::from(read_cs() & !7)),
        (VMCS_HOST_SS_SELECTOR, u64::from(read_ss() & !7)),
        (VMCS_HOST_DS_SELECTOR, u64::from(read_ds() & !7)),
        (VMCS_HOST_FS_SELECTOR, u64::from(read_fs() & !7)),
        (VMCS_HOST_GS_SELECTOR, u64::from(read_gs() & !7)),
        (VMCS_HOST_TR_SELECTOR, u64::from(tr & !7)),
    ] {
        vmwrite(field, value)?;
    }
    vmwrite(VMCS_HOST_CR0, read_cr0())?;
    vmwrite(VMCS_HOST_CR3, read_cr3())?;
    vmwrite(VMCS_HOST_CR4, read_cr4())?;
    vmwrite(VMCS_HOST_FS_BASE, read_msr(GuestMsr::FsBase.index()))?;
    vmwrite(VMCS_HOST_GS_BASE, read_msr(GuestMsr::GsBase.index()))?;
    vmwrite(VMCS_HOST_TR_BASE, tr_base)?;
    vmwrite(VMCS_HOST_GDTR_BASE, gdtr_base)?;
    vmwrite(VMCS_HOST_IDTR_BASE, idtr_base)?;
    vmwrite(
        VMCS_HOST_SYSENTER_CS,
        read_msr(GuestMsr::SysenterCs.index()),
    )?;
    vmwrite(
        VMCS_HOST_SYSENTER_ESP,
        read_msr(GuestMsr::SysenterEsp.index()),
    )?;
    vmwrite(
        VMCS_HOST_SYSENTER_EIP,
        read_msr(GuestMsr::SysenterEip.index()),
    )?;
    vmwrite(VMCS_HOST_PAT, read_msr(GuestMsr::Pat.index()))?;
    vmwrite(VMCS_HOST_EFER, read_msr(GuestMsr::Efer.index()))?;
    vmwrite(
        VMCS_HOST_RIP,
        x86_64_vmexit_entry as *const () as usize as u64,
    )
}

fn active_context_pointer() -> *mut VcpuContext {
    let cpu = crate::arch::x86_64::current_cpu_index();
    let address = ACTIVE_CONTEXT
        .get(cpu)
        .map_or(0, |slot| slot.load(Ordering::Acquire));
    if address == 0 {
        crate::arch::x86_64::vm::fail_stop("VMX active-context lookup", cpu);
    }
    core::ptr::with_exposed_provenance_mut(address)
}

fn active_eptp() -> Option<u64> {
    ACTIVE_EPTP
        .get(crate::arch::x86_64::current_cpu_index())
        .map(|slot| slot.load(Ordering::Acquire))
        .filter(|value| *value != 0)
}

fn synchronize_guest_msrs(context: &mut VcpuContext) {
    let cpu = crate::arch::x86_64::current_cpu_index();
    if cpu >= MAX_CPUS {
        crate::arch::x86_64::vm::fail_stop("VMX guest-MSR list lookup", cpu);
    }
    // SAFETY: VM exit completed its store list before transferring control to
    // this dispatcher, and hardware will not access it again until VM entry.
    // Projecting from the raw array pointer borrows only this CPU's element.
    let list = unsafe { &*GUEST_MSR_LISTS.0.get().cast::<MsrList>().add(cpu) };
    context.msrs = VcpuMsrState {
        star: list.0[0].value,
        lstar: list.0[1].value,
        cstar: list.0[2].value,
        sfmask: list.0[3].value,
        kernel_gs_base: list.0[4].value,
        tsc_aux: context.msrs.tsc_aux,
    };
}

fn update_guest_msr(msr: u32, value: u64) {
    let cpu = crate::arch::x86_64::current_cpu_index();
    if cpu >= MAX_CPUS {
        crate::arch::x86_64::vm::fail_stop("VMX guest-MSR list update", cpu);
    }
    // SAFETY: This runs between VM exit and VM entry on the owning CPU, while
    // VMX hardware is not accessing the guest load/store list. Projecting from
    // the raw array pointer borrows only this CPU's element.
    let list = unsafe { &mut *GUEST_MSR_LISTS.0.get().cast::<MsrList>().add(cpu) };
    if let Some(entry) = list.0.iter_mut().find(|entry| entry.index == msr) {
        entry.value = value;
    }
}

fn save_guest_context(registers: &ExitRegisters, context: &mut VcpuContext) {
    context.general = [
        registers.rax,
        registers.rbx,
        registers.rcx,
        registers.rdx,
        read_vmcs(VMCS_GUEST_RSP),
        registers.rbp,
        registers.rsi,
        registers.rdi,
        registers.r8,
        registers.r9,
        registers.r10,
        registers.r11,
        registers.r12,
        registers.r13,
        registers.r14,
        registers.r15,
    ];
    context.instruction_pointer = read_vmcs(VMCS_GUEST_RIP);
    context.flags = read_vmcs(VMCS_GUEST_RFLAGS);
    // The assembly frame places the 512-byte FXSAVE image immediately before
    // the integer register block passed to this function.
    let source = (registers as *const ExitRegisters).cast::<u8>();
    // SAFETY: The VM-exit assembly allocated and initialized the 512-byte
    // FXSAVE image immediately before `registers`; destinations do not overlap.
    unsafe { copy_nonoverlapping(source.sub(512), context.fx_state.as_mut_ptr(), 512) };
}

#[unsafe(no_mangle)]
unsafe extern "C" fn x86_64_vmexit_dispatch(registers: *mut ExitRegisters) {
    if registers.is_null() || !registers.is_aligned() {
        crate::arch::x86_64::vm::fail_stop(
            "invalid VMX exit-register frame",
            Error::InvalidAddress,
        );
    }
    let context = active_context_pointer();
    // SAFETY: VM entry published the pinned active context for this CPU, and
    // VM exit is serialized with any future vCPU migration. This borrow ends
    // before any registered VM-exit service may borrow the containing owner.
    unsafe { synchronize_guest_msrs(&mut *context) };
    let raw_reason = read_vmcs(VMCS_EXIT_REASON) as u32;
    if raw_reason & (1 << 31) != 0 {
        crate::arch::x86_64::vm::fail_stop(
            "VM-entry failure",
            (raw_reason & 0xffff, read_vmcs(VMCS_EXIT_QUALIFICATION)),
        );
    }
    let reason = raw_reason & 0xffff;
    match reason {
        EXIT_EXTERNAL_INTERRUPT => handle_external_interrupt(),
        EXIT_INTERRUPT_WINDOW => {}
        EXIT_CPUID => {
            // SAFETY: This architecture-local branch exclusively owns the
            // assembly register frame and invokes no registered service.
            unsafe { emulate_cpuid(&mut *registers) };
            advance_rip();
        }
        EXIT_HLT => advance_rip(),
        EXIT_IO => {
            emulate_io(registers);
            advance_rip();
        }
        // SAFETY: The active-context pointer remains pinned and no mutable
        // context borrow is live in this mutually exclusive exit branch.
        EXIT_RDMSR => unsafe { handle_rdmsr(&mut *registers, &*context) },
        // SAFETY: VM-exit dispatch exclusively owns the active vCPU here.
        EXIT_WRMSR => unsafe { handle_wrmsr(&*registers, &mut *context) },
        EXIT_EPT_VIOLATION => handle_ept_violation(),
        EXIT_EPT_MISCONFIGURATION => crate::arch::x86_64::vm::fail_stop(
            "EPT misconfiguration",
            (
                read_vmcs(VMCS_GUEST_PHYSICAL_ADDRESS),
                read_vmcs(VMCS_EXIT_QUALIFICATION),
            ),
        ),
        _ => crate::arch::x86_64::vm::fail_stop("unhandled VMX exit", reason),
    }
    prepare_guest_interrupt();
    // SAFETY: No registered service retains backend state. VM-exit dispatch
    // exclusively owns both the assembly frame and active pinned context.
    unsafe { save_guest_context(&*registers, &mut *context) };
}

fn handle_rdmsr(registers: &mut ExitRegisters, context: &VcpuContext) {
    let msr = registers.rcx as u32;
    let Some(value) = read_guest_msr(msr, context) else {
        inject_general_protection();
        return;
    };
    registers.rax = value as u32 as u64;
    registers.rdx = (value >> 32) as u32 as u64;
    advance_rip();
}

fn read_guest_msr(msr: u32, context: &VcpuContext) -> Option<u64> {
    Some(match GuestMsr::decode(msr)? {
        GuestMsr::SysenterCs => read_vmcs(VMCS_GUEST_SYSENTER_CS),
        GuestMsr::SysenterEsp => read_vmcs(VMCS_GUEST_SYSENTER_ESP),
        GuestMsr::SysenterEip => read_vmcs(VMCS_GUEST_SYSENTER_EIP),
        GuestMsr::Efer => read_vmcs(VMCS_GUEST_EFER),
        GuestMsr::FsBase => read_vmcs(VMCS_GUEST_FS_BASE),
        GuestMsr::GsBase => read_vmcs(VMCS_GUEST_GS_BASE),
        GuestMsr::ApicBase => 0,
        GuestMsr::TimestampCounter => read_msr(GuestMsr::TimestampCounter.index()),
        GuestMsr::Pat => read_vmcs(VMCS_GUEST_PAT),
        GuestMsr::Star => context.msrs.star,
        GuestMsr::Lstar => context.msrs.lstar,
        GuestMsr::Cstar => context.msrs.cstar,
        GuestMsr::Sfmask => context.msrs.sfmask,
        GuestMsr::KernelGsBase => context.msrs.kernel_gs_base,
        GuestMsr::TscAux => context.msrs.tsc_aux,
    })
}

fn handle_wrmsr(registers: &ExitRegisters, context: &mut VcpuContext) {
    let msr = registers.rcx as u32;
    let value = (registers.rax as u32 as u64) | ((registers.rdx as u32 as u64) << 32);
    if !write_guest_msr(msr, value, context) {
        inject_general_protection();
        return;
    }
    update_guest_msr(msr, value);
    advance_rip();
}

fn write_guest_msr(msr: u32, value: u64, context: &mut VcpuContext) -> bool {
    let Some(msr) = GuestMsr::decode(msr) else {
        return false;
    };
    match msr {
        GuestMsr::SysenterCs => {
            write_vmcs(VMCS_GUEST_SYSENTER_CS, value);
        }
        GuestMsr::SysenterEsp => {
            write_vmcs(VMCS_GUEST_SYSENTER_ESP, value);
        }
        GuestMsr::SysenterEip => {
            write_vmcs(VMCS_GUEST_SYSENTER_EIP, value);
        }
        GuestMsr::Efer => {
            write_vmcs(VMCS_GUEST_EFER, value);
        }
        GuestMsr::FsBase => {
            write_vmcs(VMCS_GUEST_FS_BASE, value);
        }
        GuestMsr::GsBase => {
            write_vmcs(VMCS_GUEST_GS_BASE, value);
        }
        GuestMsr::Pat => {
            write_vmcs(VMCS_GUEST_PAT, value);
        }
        GuestMsr::Star => context.msrs.star = value,
        GuestMsr::Lstar => context.msrs.lstar = value,
        GuestMsr::Cstar => context.msrs.cstar = value,
        GuestMsr::Sfmask => context.msrs.sfmask = value,
        GuestMsr::KernelGsBase => context.msrs.kernel_gs_base = value,
        GuestMsr::TscAux => context.msrs.tsc_aux = value,
        GuestMsr::TimestampCounter | GuestMsr::ApicBase => return false,
    }
    true
}

fn inject_general_protection() {
    write_vmcs(VMCS_CTRL_ENTRY_EXCEPTION_ERROR, 0);
    write_vmcs(
        VMCS_CTRL_ENTRY_INTERRUPT,
        INTERRUPTION_VALID
            | INTERRUPTION_ERROR_CODE
            | INTERRUPTION_HARDWARE_EXCEPTION
            | EXCEPTION_GENERAL_PROTECTION,
    );
}

fn handle_ept_violation() {
    let qualification = read_vmcs(VMCS_EXIT_QUALIFICATION);
    let address = read_vmcs(VMCS_GUEST_PHYSICAL_ADDRESS);
    let violation = EptViolation::decode(qualification);
    let access = match violation.access {
        EptAccess::Read => MemoryAccess::Read,
        EptAccess::Write => MemoryAccess::Write,
        EptAccess::Execute => MemoryAccess::Execute,
    };
    let fault = GuestMemoryFault::new(
        GuestPhysicalAddress::new(address),
        access,
        violation.during_page_walk,
    );
    match crate::arch::vm::dispatch_memory_fault(fault) {
        MemoryFaultAction::Retry => {}
        MemoryFaultAction::ForwardToDevice => crate::arch::x86_64::vm::fail_stop(
            "unhandled EPT device access",
            (address, qualification),
        ),
        MemoryFaultAction::Stop => crate::arch::x86_64::vm::fail_stop(
            "EPT fault policy stopped the guest",
            (address, qualification),
        ),
    }
}

#[unsafe(no_mangle)]
extern "C" fn x86_64_vmx_instruction_failure() -> ! {
    match vmread(VMCS_INSTRUCTION_ERROR) {
        Ok(error) => crate::arch::x86_64::vm::fail_stop("VMX instruction", error),
        Err(error) => {
            crate::arch::x86_64::vm::fail_stop("VMX instruction without VMCS error", error)
        }
    }
}

fn handle_external_interrupt() {
    let info = read_vmcs(VMCS_EXIT_INTERRUPT_INFO);
    if info & INTERRUPTION_VALID != 0 {
        let vector = info as u32 & 0xff;
        // VMX reports host external interrupts directly in the VM-exit
        // information field instead of entering through the IDT. Consume
        // architecture-private vectors here before VM or kernel policy sees
        // them. The private handler completes the local APIC interrupt, so
        // this path must not dispatch or acknowledge it again.
        if vector == crate::arch::x86_64::platform::KERNEL_RPC_VECTOR {
            crate::arch::x86_64::irq::controller::end_local_interrupt();
            let action = crate::arch::irq::service_kernel_rpc_interrupt(
                hyper::hal::interrupt::InterruptOrigin::Guest,
            );
            if !matches!(
                action,
                hyper::hal::interrupt::EntryAction::Resume { postlude: None }
            ) {
                crate::arch::irq::stop_entry(crate::arch::exception::capture_crash_context())
            }
            return;
        }
        match crate::arch::irq::dispatch_entry(
            hyper::hal::interrupt::InterruptId::new(vector),
            hyper::hal::interrupt::InterruptOrigin::Guest,
            &crate::hal::irq::InterruptFrame::unavailable(),
        ) {
            hyper::hal::interrupt::EntryAction::Resume { postlude } => {
                // VM exits remain cooperative until x86 provides a qualified
                // IRQ-tail continuation and vCPU teardown boundary.
                let _ = postlude;
            }
            hyper::hal::interrupt::EntryAction::StopGuest { postlude: _ } => {
                // VMX cannot consume this action until guest exit unwinds to a
                // typed scheduler-owned continuation. Fail closed if policy
                // ever publishes it without that capability.
                crate::arch::irq::stop_entry(crate::arch::exception::capture_crash_context())
            }
            hyper::hal::interrupt::EntryAction::Stop => {
                crate::arch::irq::stop_entry(crate::arch::exception::capture_crash_context())
            }
        }
        if vector == crate::arch::x86_64::platform::TIMER_VECTOR {
            timer_pending().store(true, Ordering::Release);
        }
    }
}

fn prepare_guest_interrupt() {
    let mut primary = read_vmcs(VMCS_CTRL_PRIMARY) as u32;
    if read_vmcs(VMCS_CTRL_ENTRY_INTERRUPT) & INTERRUPTION_VALID != 0 {
        primary &= !PRIMARY_INTERRUPT_WINDOW;
        write_vmcs(VMCS_CTRL_PRIMARY, u64::from(primary));
        return;
    }
    let timer_pending = timer_pending();
    let (vector, consumes_timer) =
        match crate::arch::vm::query_pending_interrupt(timer_pending.load(Ordering::Acquire)) {
            PendingInterruptAction::None => {
                primary &= !PRIMARY_INTERRUPT_WINDOW;
                write_vmcs(VMCS_CTRL_PRIMARY, u64::from(primary));
                return;
            }
            PendingInterruptAction::Inject {
                vector,
                consumes_timer,
            } => (vector, consumes_timer),
            PendingInterruptAction::Stop => crate::arch::x86_64::vm::fail_stop(
                "x86 interrupt-routing policy stopped the guest",
                (),
            ),
        };
    let flags = read_vmcs(VMCS_GUEST_RFLAGS);
    let blocked = read_vmcs(VMCS_GUEST_INTERRUPTIBILITY) != 0;
    if flags & (1 << 9) != 0 && !blocked {
        write_vmcs(
            VMCS_CTRL_ENTRY_INTERRUPT,
            INTERRUPTION_VALID | u64::from(vector),
        );
        if consumes_timer {
            timer_pending.store(false, Ordering::Release);
        }
        primary &= !PRIMARY_INTERRUPT_WINDOW;
    } else {
        primary |= PRIMARY_INTERRUPT_WINDOW;
    }
    write_vmcs(VMCS_CTRL_PRIMARY, u64::from(primary));
}

fn timer_pending() -> &'static AtomicBool {
    match TIMER_PENDING.get(crate::arch::x86_64::current_cpu_index()) {
        Some(pending) => pending,
        None => crate::arch::x86_64::vm::fail_stop("VMX timer CPU lookup", Error::InvalidCpu),
    }
}

fn emulate_cpuid(registers: &mut ExitRegisters) {
    let requested_leaf = registers.rax as u32;
    let requested_subleaf = registers.rcx as u32;
    if requested_leaf == 0x4000_0000 {
        write_cpuid_result(registers, hypervisor_cpuid());
        return;
    }
    if (0x4000_0001..0x5000_0000).contains(&requested_leaf) {
        write_cpuid_result(registers, CpuidResult::ZERO);
        return;
    }
    let leaf = core::arch::x86_64::__cpuid_count(requested_leaf, requested_subleaf);
    let value = sanitize_cpuid(
        requested_leaf,
        requested_subleaf,
        CpuidResult {
            eax: leaf.eax,
            ebx: leaf.ebx,
            ecx: leaf.ecx,
            edx: leaf.edx,
        },
    );
    write_cpuid_result(registers, value);
}

fn write_cpuid_result(registers: &mut ExitRegisters, value: CpuidResult) {
    registers.rax = u64::from(value.eax);
    registers.rbx = u64::from(value.ebx);
    registers.rcx = u64::from(value.ecx);
    registers.rdx = u64::from(value.edx);
}

fn emulate_io(registers: *mut ExitRegisters) {
    let qualification = read_vmcs(VMCS_EXIT_QUALIFICATION);
    let Some(exit) = IoExit::decode(qualification) else {
        crate::arch::x86_64::vm::fail_stop("invalid VMX I/O access size", qualification)
    };
    if exit.string || exit.repeat {
        crate::arch::x86_64::vm::fail_stop("unsupported VMX string I/O", qualification);
    }
    let Some(width) = PortIoWidth::from_bytes(exit.size) else {
        crate::arch::x86_64::vm::fail_stop("invalid VMX I/O access size", exit.size)
    };
    // SAFETY: The dispatcher supplies its valid assembly frame. Copy policy
    // input before invoking the registered service and retain no frame borrow.
    let accumulator = unsafe { core::ptr::addr_of!((*registers).rax).read() };
    let operation = match exit.direction {
        IoDirection::Input => PortIoOperation::Input,
        IoDirection::Output => PortIoOperation::Output(accumulator as u32),
    };
    let event = PortIoExit::new(exit.port, width, operation);
    let action = crate::arch::vm::dispatch_port_io(event);
    match complete_port_io(accumulator, event, action) {
        Ok(PortIoCompletion::Input(merged)) => {
            // SAFETY: Policy returned without retaining the frame; apply its
            // owned result through a fresh raw-pointer projection.
            unsafe { core::ptr::addr_of_mut!((*registers).rax).write(merged) };
        }
        Ok(PortIoCompletion::Output) => {}
        Err(PortIoCompletionError::PolicyStopped) => {
            crate::arch::x86_64::vm::fail_stop("VMX I/O policy stopped the guest", event)
        }
        Err(PortIoCompletionError::ActionMismatch | PortIoCompletionError::InvalidWidth) => {
            crate::arch::x86_64::vm::fail_stop("invalid VMX I/O policy completion", (event, action))
        }
    }
}

fn advance_rip() {
    let rip = read_vmcs(VMCS_GUEST_RIP);
    let length = read_vmcs(VMCS_EXIT_INSTRUCTION_LENGTH);
    write_vmcs(VMCS_GUEST_RIP, rip.wrapping_add(length));
}

fn read_vmcs(field: u64) -> u64 {
    match vmread(field) {
        Ok(value) => value,
        Err(error) => crate::arch::x86_64::vm::fail_stop("VMCS read", (field, error)),
    }
}

fn write_vmcs(field: u64, value: u64) {
    if let Err(error) = vmwrite(field, value) {
        crate::arch::x86_64::vm::fail_stop("VMCS write", (field, error));
    }
}

fn adjust_control(desired: u32, msr: u32) -> Result<u32, Error> {
    ControlCapability::decode(read_msr(msr))
        .apply(desired)
        .ok_or(Error::InvalidControl)
}

fn control_msr(legacy: u32, true_controls: u32) -> u32 {
    if read_msr(IA32_VMX_BASIC) & (1 << 55) != 0 {
        true_controls
    } else {
        legacy
    }
}

fn descriptor_base(gdt: u64, selector: u16) -> Result<u64, Error> {
    let address = gdt
        .checked_add(u64::from(selector & !7))
        .ok_or(Error::InvalidAddress)? as *const u64;
    // SAFETY: The live host GDTR identifies a mapped GDT, and the selected
    // system descriptor occupies these two possibly unaligned u64 words.
    let low = unsafe { read_unaligned(address) };
    // SAFETY: A 64-bit TSS system descriptor is 16 bytes, so the upper word is
    // part of the same mapped GDT entry.
    let high = unsafe { read_unaligned(address.add(1)) };
    Ok(((low >> 16) & 0xffff)
        | (((low >> 32) & 0xff) << 16)
        | (((low >> 56) & 0xff) << 24)
        | ((high & 0xffff_ffff) << 32))
}

pub(crate) fn activate_ept(root: u64) {
    let eptp = root | 6 | (3 << 3);
    let cpu = crate::arch::x86_64::current_cpu_index();
    let Some(slot) = ACTIVE_EPTP.get(cpu) else {
        crate::arch::x86_64::vm::fail_stop("VMX EPT CPU lookup", Error::InvalidCpu);
    };
    slot.store(eptp, Ordering::Release);
    EPT_INVALIDATION_PENDING[cpu].store(true, Ordering::Release);
}

pub(crate) fn invalidate_ept(root: u64) -> Result<(), Error> {
    let active = VMX_ACTIVE
        .get(crate::arch::x86_64::current_cpu_index())
        .ok_or(Error::InvalidCpu)?;
    if !active.load(Ordering::Acquire) {
        return Err(Error::VmxInactive);
    }
    invept_single_context(root | 6 | (3 << 3))
}

fn kernel_physical(virtual_address: usize) -> Result<u64, Error> {
    crate::arch::x86_64::memory::kernel_image_physical_address(virtual_address)
        .ok_or(Error::InvalidAddress)
}
