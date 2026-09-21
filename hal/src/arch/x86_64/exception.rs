// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

use core::arch::asm;
use core::cell::UnsafeCell;

use hyper::hal::interrupt::InterruptId;
use hyper::sync::atomic::{AtomicBool, Ordering};

const MAX_CPUS: usize = hyper::config::MAX_CPUS as usize;
const IDT_ENTRIES: usize = 256;
pub const NO_EXCEPTION_VECTOR: u64 = u64::MAX;

#[repr(C, packed)]
#[derive(Clone, Copy)]
struct IdtEntry {
    offset_low: u16,
    selector: u16,
    ist: u8,
    attributes: u8,
    offset_middle: u16,
    offset_high: u32,
    reserved: u32,
}

impl IdtEntry {
    const EMPTY: Self = Self {
        offset_low: 0,
        selector: 0,
        ist: 0,
        attributes: 0,
        offset_middle: 0,
        offset_high: 0,
        reserved: 0,
    };

    fn interrupt(handler: u64, ist: u8) -> Self {
        Self {
            offset_low: handler as u16,
            selector: 0x20,
            ist,
            attributes: 0x8e,
            offset_middle: (handler >> 16) as u16,
            offset_high: (handler >> 32) as u32,
            reserved: 0,
        }
    }
}

#[repr(C, packed)]
struct DescriptorTablePointer {
    limit: u16,
    base: u64,
}

#[repr(C, packed)]
struct TaskStateSegment {
    reserved_0: u32,
    privilege_stack: [u64; 3],
    reserved_1: u64,
    interrupt_stack: [u64; 7],
    reserved_2: u64,
    reserved_3: u16,
    io_bitmap_offset: u16,
}

impl TaskStateSegment {
    const EMPTY: Self = Self {
        reserved_0: 0,
        privilege_stack: [0; 3],
        reserved_1: 0,
        interrupt_stack: [0; 7],
        reserved_2: 0,
        reserved_3: 0,
        io_bitmap_offset: core::mem::size_of::<Self>() as u16,
    };
}

struct DescriptorStorage(UnsafeCell<[[u64; 7]; MAX_CPUS]>);
// SAFETY: Each GDT element is written and loaded only by its matching CPU
// before that CPU enables runtime interrupts, then remains immutable.
unsafe impl Sync for DescriptorStorage {}
struct TaskStateStorage(UnsafeCell<[TaskStateSegment; MAX_CPUS]>);
// SAFETY: The boot CPU initializes each TSS, then publishes its readiness with
// a release store. The target CPU performs an acquire load before LTR and
// runtime interrupt delivery. Its IST fields remain immutable thereafter.
unsafe impl Sync for TaskStateStorage {}

static GDTS: DescriptorStorage = DescriptorStorage(UnsafeCell::new([[0; 7]; MAX_CPUS]));
static TASK_STATES: TaskStateStorage = TaskStateStorage(UnsafeCell::new(
    [const { TaskStateSegment::EMPTY }; MAX_CPUS],
));

struct IdtStorage(UnsafeCell<[IdtEntry; IDT_ENTRIES]>);
// SAFETY: The boot CPU initializes the IDT once before publishing `INSTALLED`.
// Secondary CPUs acquire that publication before loading the table, and no CPU
// mutates the IDT after publication.
unsafe impl Sync for IdtStorage {}

static IDT: IdtStorage = IdtStorage(UnsafeCell::new([IdtEntry::EMPTY; IDT_ENTRIES]));
static INSTALLED: AtomicBool = AtomicBool::new(false);
static TASK_STATE_CLAIMED: [AtomicBool; MAX_CPUS] = [const { AtomicBool::new(false) }; MAX_CPUS];
static TASK_STATE_READY: [AtomicBool; MAX_CPUS] = [const { AtomicBool::new(false) }; MAX_CPUS];

#[repr(C)]
pub(crate) struct ExceptionFrame {
    // Register order follows the push sequence as observed from the final RSP.
    general: [u64; 15],
    vector: u64,
    error: u64,
    rip: u64,
    cs: u64,
    rflags: u64,
    rsp: u64,
    ss: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(C, align(16))]
pub struct CrashContext {
    pub general: [u64; 16],
    general_valid: u64,
    pub stack_pointer: u64,
    pub program_counter: u64,
    pub processor_state: u64,
    pub syndrome: u64,
    pub fault_address: u64,
    pub exception_vector: u64,
    pub hardware_id: u64,
    pub interrupt_mask: u64,
    pub cr3: u64,
}

impl CrashContext {
    pub const FRAME_POINTER_REGISTER: usize = 10;
    pub const GENERAL_REGISTER_COUNT: usize = 16;

    pub const fn general_is_valid(&self, register: usize) -> bool {
        register < 16 && self.general_valid & (1 << register) != 0
    }

    pub const fn has_exception_frame(&self) -> bool {
        self.exception_vector != NO_EXCEPTION_VECTOR
    }

    pub fn describe_cpu_header(
        &self,
        cpu: usize,
        role: &str,
        mut emit: impl FnMut(core::fmt::Arguments<'_>),
    ) {
        emit(format_args!(
            "CPU {cpu}: {role}, APIC ID {:#x}, RFLAGS {:#x}",
            self.hardware_id, self.interrupt_mask
        ));
    }

    pub fn describe_architecture_registers(&self, mut emit: impl FnMut(core::fmt::Arguments<'_>)) {
        if self.has_exception_frame() {
            emit(format_args!(
                "vector: {:#x}  error: {:#018x}  CR2: {:#018x}",
                self.exception_vector, self.syndrome, self.fault_address
            ));
        } else {
            emit(format_args!(
                "origin: software panic (no architectural exception frame)"
            ));
        }
        emit(format_args!("CR3: {:#018x}", self.cr3));
    }

    /// Reads the frame record beginning at `frame`.
    ///
    /// # Safety
    ///
    /// The complete `[bottom, top)` range must remain mapped, readable, and
    /// pinned for this call. `frame` must refer to a live frame record in that
    /// range, and no concurrent agent may modify either word being read.
    pub unsafe fn previous_stack_frame(
        frame: usize,
        bottom: usize,
        top: usize,
    ) -> Result<Option<(usize, usize)>, crate::arch::exception::InvalidStackFrame> {
        if frame & 0xf != 0 || frame < bottom || frame.checked_add(16).is_none_or(|end| end > top) {
            return Err(crate::arch::exception::InvalidStackFrame);
        }
        // SAFETY: Both words lie inside the validated, pinned kernel stack.
        let previous =
            unsafe { core::ptr::read_volatile(core::ptr::with_exposed_provenance::<usize>(frame)) };
        // SAFETY: The second word is also inside the validated frame record.
        let link = unsafe {
            core::ptr::read_volatile(core::ptr::with_exposed_provenance::<usize>(
                frame + core::mem::size_of::<usize>(),
            ))
        };
        Ok((link != 0).then_some((previous, link)))
    }
}

pub fn capture_crash_context() -> CrashContext {
    let frame_pointer: u64;
    let program_counter: u64;
    let rsp: u64;
    let rflags: u64;
    let cr2: u64;
    let cr3: u64;
    // SAFETY: This snapshots CPL0 registers and balances PUSHFQ with POP.
    unsafe {
        asm!(
            "mov rax, rbp",
            "lea rcx, [rip + 2f]",
            "2:",
            "mov rdx, rsp",
            "pushfq",
            "pop rsi",
            "mov rdi, cr2",
            "mov r8, cr3",
            lateout("rax") frame_pointer,
            lateout("rcx") program_counter,
            lateout("rdx") rsp,
            lateout("rsi") rflags,
            lateout("rdi") cr2,
            lateout("r8") cr3,
        )
    };
    let mut general = [0; 16];
    general[4] = rsp;
    general[5] = frame_pointer;
    CrashContext {
        general,
        general_valid: (1 << 4) | (1 << 5),
        stack_pointer: rsp,
        program_counter,
        processor_state: rflags,
        syndrome: 0,
        fault_address: cr2,
        exception_vector: NO_EXCEPTION_VECTOR,
        hardware_id: super::current_hardware_id(),
        interrupt_mask: rflags,
        cr3,
    }
}

/// Installs the assembly-visible IST stacks for one CPU.
///
/// # Safety
///
/// Both ranges must be pinned, exclusively reserved stack mappings that remain
/// live while the CPU is online. This must complete before the target CPU can
/// load its TSS, and each CPU slot may be installed only once.
pub unsafe fn install_exception_stacks(
    cpu: usize,
    irq: (usize, usize),
    emergency: (usize, usize),
) -> Result<(), ()> {
    if cpu >= MAX_CPUS || irq.0 >= irq.1 || emergency.0 >= emergency.1 {
        return Err(());
    }
    if TASK_STATE_CLAIMED[cpu]
        .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
        .is_err()
    {
        return Err(());
    }
    // SAFETY: The successful claim gives this call exclusive access to the
    // selected element. Raw element writes avoid creating an exclusive borrow
    // of the complete array, so different CPUs may be prepared concurrently.
    let task_state = TASK_STATES
        .0
        .get()
        .cast::<TaskStateSegment>()
        .wrapping_add(cpu);
    // SAFETY: The successful claim exclusively owns this element until publication.
    unsafe {
        let interrupt_stack = core::ptr::addr_of_mut!((*task_state).interrupt_stack).cast::<u64>();
        interrupt_stack.write(irq.1 as u64);
        interrupt_stack.add(1).write(emergency.1 as u64);
    }
    TASK_STATE_READY[cpu].store(true, Ordering::Release);
    Ok(())
}

unsafe extern "C" {
    static x86_64_vector_stubs: [i32; IDT_ENTRIES];
}

/// Installs and publishes the runtime IDT.
///
/// # Safety
///
/// This must run exactly once while all secondary CPUs and local interrupts
/// are stopped. The final executable mapping and bootstrap TSS must be active.
pub unsafe fn install_runtime_vectors() {
    // SAFETY: The one-time installation contract grants exclusive IDT mutation.
    let table = unsafe { &mut *IDT.0.get() };
    let base = core::ptr::addr_of!(x86_64_vector_stubs) as usize;
    for (vector, (entry, offset)) in table
        .iter_mut()
        // SAFETY: Assembly defines exactly IDT_ENTRIES relative stub offsets.
        .zip(unsafe { x86_64_vector_stubs }.iter())
        .enumerate()
    {
        let handler = base.wrapping_add_signed(*offset as isize);
        let ist = if vector < 32 { 2 } else { 1 };
        *entry = IdtEntry::interrupt(handler as u64, ist);
    }
    INSTALLED.store(true, Ordering::Release);
    install_local_vectors();
}

pub fn install_local_vectors() {
    if !INSTALLED.load(Ordering::Acquire) {
        hyper::debug::invariant_failure("x86_64/exception::install_local_vectors invariant");
    }
    install_local_descriptors();
    // SAFETY: Acquire of INSTALLED observes completed immutable IDT initialization.
    let table = unsafe { &*IDT.0.get() };
    let pointer = DescriptorTablePointer {
        limit: (core::mem::size_of_val(table) - 1) as u16,
        base: table.as_ptr() as u64,
    };
    // SAFETY: The descriptor points to the live IDT and LIDT is valid at CPL0.
    unsafe { asm!("lidt [{}]", in(reg) &pointer, options(readonly, nostack)) };
}

fn install_local_descriptors() {
    let cpu = super::current_cpu_index();
    if cpu >= MAX_CPUS {
        hyper::debug::invariant_failure("x86_64/exception::install_local_descriptors invariant");
    }
    if !TASK_STATE_READY
        .get(cpu)
        .is_some_and(|ready| ready.load(Ordering::Acquire))
    {
        hyper::debug::invariant_failure("x86_64/exception::install_local_descriptors invariant");
    }
    // SAFETY: `cpu` is in range. Each CPU exclusively initializes its own GDT
    // element before enabling interrupts; projecting the raw array pointer
    // avoids borrowing elements owned by other CPUs.
    let table = unsafe { &mut *GDTS.0.get().cast::<[u64; 7]>().add(cpu) };
    // SAFETY: The acquire above observed completion of this CPU's TSS setup.
    // Its fields are immutable after publication.
    let task_state = unsafe { &*TASK_STATES.0.get().cast::<TaskStateSegment>().add(cpu) };
    let base = core::ptr::from_ref(task_state).addr() as u64;
    let limit = (core::mem::size_of::<TaskStateSegment>() - 1) as u64;
    table[2] = 0x00cf_9a00_0000_ffff;
    table[3] = 0x00cf_9200_0000_ffff;
    table[4] = 0x00af_9a00_0000_ffff;
    table[5] = limit | ((base & 0x00ff_ffff) << 16) | (0x89 << 40) | (((base >> 24) & 0xff) << 56);
    table[6] = base >> 32;
    let pointer = DescriptorTablePointer {
        limit: (core::mem::size_of_val(table) - 1) as u16,
        base: table.as_ptr() as u64,
    };
    // SAFETY: The descriptors are live; LGDT/LTR are valid at CPL0 during setup.
    unsafe {
        asm!(
            "lgdt [{}]",
            "mov ax, 0x28",
            "ltr ax",
            in(reg) &pointer,
            out("ax") _,
            options(readonly, nostack),
        )
    };
}

/// Installs the immutable runtime IDT and this CPU's descriptor state.
///
/// # Safety
///
/// The global IDT must already be published, this CPU's TSS and exception
/// stacks must be installed, and local interrupts must remain masked until the
/// local descriptor state has been validated.
pub unsafe fn install_local_runtime_vectors() {
    install_local_vectors();
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ValidationError {
    NotInstalled,
}

pub fn validate_runtime_vectors() -> Result<(), ValidationError> {
    INSTALLED
        .load(Ordering::Acquire)
        .then_some(())
        .ok_or(ValidationError::NotInstalled)
}

/// Validates the IDT descriptor local to the calling CPU.
pub fn validate_local_runtime_vectors() -> Result<(), ValidationError> {
    let mut pointer = DescriptorTablePointer { limit: 0, base: 0 };
    // SAFETY: SIDT stores the calling CPU's descriptor into live local storage.
    unsafe { asm!("sidt [{}]", in(reg) &mut pointer, options(nostack)) };
    // SAFETY: `pointer` is initialized above; unaligned access is required for
    // the packed architectural descriptor representation.
    let installed_base = unsafe { core::ptr::addr_of!(pointer.base).read_unaligned() };
    let expected_base = core::ptr::addr_of!(IDT.0) as u64;
    (installed_base == expected_base)
        .then_some(())
        .ok_or(ValidationError::NotInstalled)
}

unsafe extern "C" {
    fn x86_64_run_on_stack(top: usize, callback: extern "C" fn(usize) -> !, argument: usize) -> !;
}

/// Transfers control to `callback` on the current CPU's emergency IST stack.
///
/// # Safety
///
/// The installed emergency stack must not already be active, and callback must
/// not return or retain references into the abandoned stack.
pub unsafe fn run_on_emergency_stack(callback: extern "C" fn(usize) -> !, argument: usize) -> ! {
    let cpu = super::current_cpu_index();
    let top = if TASK_STATE_READY
        .get(cpu)
        .is_some_and(|ready| ready.load(Ordering::Acquire))
    {
        // SAFETY: `cpu` was validated by the matching ready-slot lookup. The
        // acquire observed completed initialization, and this element remains
        // immutable thereafter.
        let state = unsafe { &*TASK_STATES.0.get().cast::<TaskStateSegment>().add(cpu) };
        state.interrupt_stack[1] as usize
    } else {
        0
    };
    if top == 0 {
        callback(argument)
    }
    // SAFETY: The function contract guarantees this emergency stack is unused.
    unsafe { x86_64_run_on_stack(top, callback, argument) }
}

pub fn bootstrap_stack_bounds(stack_pointer: u64) -> Option<(usize, usize)> {
    unsafe extern "C" {
        static __boot_stack_bottom: u8;
        static __boot_stack_top: u8;
    }
    let bottom = core::ptr::addr_of!(__boot_stack_bottom) as usize;
    let top = core::ptr::addr_of!(__boot_stack_top) as usize;
    (stack_pointer >= bottom as u64 && stack_pointer < top as u64).then_some((bottom, top))
}

#[unsafe(no_mangle)]
extern "C" fn x86_64_vector_dispatch(frame: &mut ExceptionFrame) {
    let vector = frame.vector as u32;
    if vector >= 32 {
        if vector == super::platform::KERNEL_RPC_VECTOR {
            super::interrupt_controller::end_local_interrupt();
            let action = crate::arch::irq::service_kernel_rpc_interrupt(
                hyper::hal::interrupt::InterruptOrigin::Host,
            );
            if !matches!(
                action,
                hyper::hal::interrupt::EntryAction::Resume { postlude: None }
            ) {
                crate::arch::irq::stop_entry(exception_crash_context(frame))
            }
            return;
        }
        super::virtualization::observe_host_interrupt(vector);
        match crate::arch::irq::dispatch_entry(
            InterruptId::new(vector),
            hyper::hal::interrupt::InterruptOrigin::Host,
        ) {
            hyper::hal::interrupt::EntryAction::Resume { postlude } => {
                // This architecture retains the request for a cooperative
                // point until it provides a qualified IRQ-tail continuation.
                let _ = postlude;
            }
            hyper::hal::interrupt::EntryAction::StopGuest { postlude: _ } => {
                // x86 has not published a qualified typed guest-stop unwind.
                // Treating this action as resumable would lose kernel policy.
                crate::arch::irq::stop_entry(exception_crash_context(frame))
            }
            hyper::hal::interrupt::EntryAction::Stop => {
                crate::arch::irq::stop_entry(exception_crash_context(frame))
            }
        }
        return;
    }
    let context = exception_crash_context(frame);
    crate::arch::exception::fatal(
        context,
        format_args!(
            "fatal x86 exception: vector {}, error {:#x}, RIP {:#x}, CR2 {:#x}, RFLAGS {:#x}",
            frame.vector, frame.error, frame.rip, context.fault_address, frame.rflags
        ),
    )
}

fn exception_crash_context(frame: &ExceptionFrame) -> CrashContext {
    let mut context = capture_crash_context();
    context.program_counter = frame.rip;
    context.stack_pointer = frame.rsp;
    context.processor_state = frame.rflags;
    context.syndrome = frame.error;
    context.exception_vector = frame.vector;
    context.general = [
        frame.general[14],
        frame.general[13],
        frame.general[12],
        frame.general[11],
        frame.rsp,
        frame.general[10],
        frame.general[9],
        frame.general[8],
        frame.general[7],
        frame.general[6],
        frame.general[5],
        frame.general[4],
        frame.general[3],
        frame.general[2],
        frame.general[1],
        frame.general[0],
    ];
    context.general_valid = u16::MAX as u64;
    context
}
