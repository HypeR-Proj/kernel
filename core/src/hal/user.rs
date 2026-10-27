// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Native-user entry and translation ownership contracts.

use core::marker::PhantomData;
use core::ptr::NonNull;

use crate::abi::native::{NativeInvocation, NativeResult};

/// One resident 4 KiB user leaf; descriptor encoding remains architecture-owned.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MappingPage {
    pub address: u64,
    pub physical: crate::mm::PhysicalAddress,
    pub readable: bool,
    pub writable: bool,
    pub executable: bool,
}

/// Ordered edits against an immutable, retained prior translation hierarchy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MappingUpdate {
    Unmap { address: u64, length: u64 },
    Map(MappingPage),
}

/// Owned register evidence copied only after a Native machine run has ended.
/// Architectural register layouts remain explicit; unused words are zero.
#[derive(Clone, Copy)]
pub struct UserRegisterSnapshot {
    pub machine: u32,
    pub pc: u64,
    pub sp: u64,
    pub tls: u64,
    pub processor_state: u64,
    pub general: [u64; 32],
    pub vector: [u64; 64],
    pub fp_control: [u64; 2],
}

impl UserRegisterSnapshot {
    pub const EMPTY: Self = Self {
        machine: 0,
        pc: 0,
        sp: 0,
        tls: 0,
        processor_state: 0,
        general: [0; 32],
        vector: [0; 64],
        fp_control: [0; 2],
    };
}

/// Result of a Native syscall reached directly from synchronous exception entry.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeCallAction {
    /// Encode the completed result in the interrupted frame and return to EL0.
    Return(NativeResult),
    /// Close this run without returning to userspace. Policy owns the reason.
    Stop,
}

/// Thread-owned policy callable from a Native synchronous exception.
///
/// # Safety
///
/// Entry and return have local interrupts masked. The implementation enters
/// ordinary interruptible, preemptible Thread context before dispatching and
/// may block or migrate. The scheduler must suspend the architecture's active
/// Native publication and translation before every actual context switch.
/// Returning [`NativeCallAction::Return`] requires an admitted translation on
/// the current CPU and a completed stop/preemption check. No CPU-affine guard
/// or borrow may span the interruptible portion. The handler and its owning
/// Thread remain alive until the machine run has closed.
pub unsafe trait NativeCallHandler {
    /// Handles one owned invocation on the current Thread's kernel stack.
    ///
    /// # Safety
    ///
    /// The caller must satisfy this trait's entry, lifetime, and scheduler
    /// suspension contract. No architecture-frame reference crosses this call.
    unsafe fn dispatch(&self, invocation: NativeInvocation) -> NativeCallAction;
}

/// Borrowed, type-erased Native exception service for one machine run.
///
/// The service follows its Thread across blocking and migration. Architecture
/// entry publishes its address only while that Thread owns the CPU's Native
/// machine state, and withdraws it before switching to another Thread. The
/// service itself and its handler borrow remain live until the run closes.
pub struct NativeCallService<'handler> {
    handler: NonNull<()>,
    dispatch: unsafe fn(NonNull<()>, NativeInvocation) -> NativeCallAction,
    _lifetime: PhantomData<&'handler ()>,
    _thread_owned: PhantomData<*mut ()>,
}

impl<'handler> NativeCallService<'handler> {
    pub fn new<T: NativeCallHandler>(handler: &'handler T) -> Self {
        unsafe fn dispatch<T: NativeCallHandler>(
            handler: NonNull<()>,
            invocation: NativeInvocation,
        ) -> NativeCallAction {
            // SAFETY: `NativeCallService::new` stores the address of a live T,
            // and the service lifetime prevents use after that borrow ends.
            let handler = unsafe { handler.cast::<T>().as_ref() };
            // SAFETY: The erased entry point is reached only through
            // `NativeCallService::handle`, which forwards its caller contract.
            unsafe { handler.dispatch(invocation) }
        }

        Self {
            handler: NonNull::from(handler).cast(),
            dispatch: dispatch::<T>,
            _lifetime: PhantomData,
            _thread_owned: PhantomData,
        }
    }

    /// Invokes the borrowed handler for the active Native machine run.
    ///
    /// # Safety
    ///
    /// The caller must own the Thread's machine run which published this
    /// service and satisfy [`NativeCallHandler::dispatch`]'s context contract.
    pub unsafe fn handle(&self, invocation: NativeInvocation) -> NativeCallAction {
        // SAFETY: Construction binds this function pointer and erased address
        // to the same handler type for the full service lifetime. The caller
        // supplies the execution-context preconditions forwarded above.
        unsafe { (self.dispatch)(self.handler, invocation) }
    }
}

/// Kernel-owned identity bound to one admitted native-user run.
///
/// These values are diagnostic and stale-return protection, not authority.
/// Process policy mints a fresh nonzero run generation only after closing all
/// fallible preparation and keeps it admitted until architecture exit is
/// acknowledged.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UserRunBinding {
    thread: u64,
    image_generation: u64,
    run_generation: u64,
}

impl UserRunBinding {
    pub const fn new(thread: u64, image_generation: u64, run_generation: u64) -> Option<Self> {
        if thread == 0 || image_generation == 0 || run_generation == 0 {
            None
        } else {
            Some(Self {
                thread,
                image_generation,
                run_generation,
            })
        }
    }

    pub const fn thread(self) -> u64 {
        self.thread
    }

    pub const fn image_generation(self) -> u64 {
        self.image_generation
    }

    pub const fn run_generation(self) -> u64 {
        self.run_generation
    }
}

/// Architecture-neutral classification of a contained native-user fault.
///
/// Page-fault variants identify accesses which may need demand paging or COW,
/// not permission to resume unconditionally. The kernel must validate logical
/// mapping access and resolve backing or publication before retrying. Some
/// architectures cannot distinguish missing pages from permission failures.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UserFaultKind {
    InstructionAbort,
    DataAbort,
    /// A write translation/permission fault with a valid fault address.
    WritePageFault,
    /// A read page-fault candidate with a valid fault address.
    ReadPageFault,
    /// An instruction page-fault candidate with a valid fault address.
    ExecutePageFault,
    Alignment,
    IllegalInstruction,
    SystemAccess,
    Breakpoint,
    OtherSynchronous,
}

/// Owned fault report copied out of an architecture-private exception frame.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UserFault {
    kind: UserFaultKind,
    syndrome: u64,
    address: u64,
    program_counter: u64,
}

impl UserFault {
    pub const fn new(
        kind: UserFaultKind,
        syndrome: u64,
        address: u64,
        program_counter: u64,
    ) -> Self {
        Self {
            kind,
            syndrome,
            address,
            program_counter,
        }
    }

    pub const fn kind(self) -> UserFaultKind {
        self.kind
    }

    pub const fn syndrome(self) -> u64 {
        self.syndrome
    }

    pub const fn address(self) -> u64 {
        self.address
    }

    pub const fn program_counter(self) -> u64 {
        self.program_counter
    }
}

/// Owner which retains every machine root currently installed by its active
/// local tokens, including across acknowledged immutable-root replacement.
///
/// # Safety
///
/// Implementors must not release a root or translation identifier while any
/// active token can still name it. Replacement may retire an old root only
/// after every active CPU has installed the successor and acknowledged.
pub unsafe trait UserTranslationOwner: Sync {}
