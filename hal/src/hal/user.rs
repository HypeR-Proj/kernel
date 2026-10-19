// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Selected native-user machine capability.
//!
//! Process, syscall, rights, and mapping ownership remain kernel policy. The
//! facade publishes address-space limits, root construction and activation,
//! and the narrow external-memory copy mechanism used for machine-visible
//! user pages. Kernel-owned tokens retain policy, residency, and lifetime.

use core::marker::PhantomData;

use hyper::cpu::CpuIndex;
pub use hyper::hal::user::UserRegisterSnapshot;
use hyper::hal::user::{NativeCallService, UserFault, UserRunBinding};
use hyper::mm::PhysicalAddress;
use hyper::sync::InterruptMaskGuard;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ExposedCopyError;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AddressSpaceError {
    InvalidCpu,
    InvalidAddressLimit,
    #[cfg(not(any(CONFIG_ARCH_AARCH64, CONFIG_ARCH_RISCV64)))]
    Unsupported,
    #[cfg(any(CONFIG_ARCH_AARCH64, CONFIG_ARCH_RISCV64))]
    Backend(crate::arch::user::UserAddressSpaceError),
    #[cfg(any(CONFIG_ARCH_AARCH64, CONFIG_ARCH_RISCV64))]
    Contract(crate::arch::user::UserMachineContractError),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UserEntryError {
    #[cfg_attr(
        not(any(CONFIG_ARCH_AARCH64, CONFIG_ARCH_RISCV64)),
        allow(dead_code, reason = "secondary Native entry is unsupported")
    )]
    InvalidContext,
    InterruptsEnabled,
    Unsupported,
    #[cfg(any(CONFIG_ARCH_AARCH64, CONFIG_ARCH_RISCV64))]
    Backend(crate::arch::user::UserEntryError),
}

/// Opaque construction policy for the selected native-user translation regime.
///
/// The plan keeps architecture layout limits and ASID width below the HAL boundary.
/// Kernel ownership supplies storage and typed identifier lifetimes.
pub struct AddressSpacePlan {
    address_limit: u64,
    application_limit: u64,
    identifier_bits: u8,
}

/// Conservative page bound for a four-level host hierarchy, including its root.
pub fn table_page_capacity(leaf_count: usize) -> Option<usize> {
    leaf_count
        .checked_mul(3)
        .and_then(|pages| pages.checked_add(1))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HostMachine {
    Aarch64,
    Riscv64,
    X86_64,
}

pub const fn host_machine() -> HostMachine {
    #[cfg(CONFIG_ARCH_AARCH64)]
    {
        HostMachine::Aarch64
    }
    #[cfg(CONFIG_ARCH_RISCV64)]
    {
        HostMachine::Riscv64
    }
    #[cfg(CONFIG_ARCH_X86_64)]
    {
        HostMachine::X86_64
    }
}

pub fn address_space_plan() -> Result<AddressSpacePlan, AddressSpaceError> {
    #[cfg(any(CONFIG_ARCH_AARCH64, CONFIG_ARCH_RISCV64))]
    {
        let address_limit =
            crate::arch::user::user_address_limit().map_err(AddressSpaceError::Contract)?;
        let application_limit = crate::arch::user::application_address_limit();
        if application_limit == 0 || application_limit >= address_limit {
            return Err(AddressSpaceError::InvalidAddressLimit);
        }
        let identifier_bits = crate::arch::user::user_translation_identifier_bits()
            .map_err(AddressSpaceError::Contract)?;
        Ok(AddressSpacePlan {
            address_limit,
            application_limit,
            identifier_bits,
        })
    }
    #[cfg(not(any(CONFIG_ARCH_AARCH64, CONFIG_ARCH_RISCV64)))]
    {
        Err(AddressSpaceError::Unsupported)
    }
}

impl AddressSpacePlan {
    /// Returns the exclusive native-user virtual-address limit.
    pub const fn address_limit(&self) -> u64 {
        self.address_limit
    }

    /// Exclusive limit for application-controlled mappings in this profile.
    /// Addresses above it remain available only to kernel-managed user mappings.
    pub const fn application_limit(&self) -> u64 {
        self.application_limit
    }

    /// ASID width admitted by the selected host-stage translation mechanism.
    pub const fn asid_bits(&self) -> u8 {
        self.identifier_bits
    }
}

#[derive(Clone, Copy)]
#[cfg_attr(not(any(CONFIG_ARCH_AARCH64, CONFIG_ARCH_RISCV64)), allow(dead_code))]
pub struct MappingPage {
    pub address: u64,
    pub physical: PhysicalAddress,
    pub readable: bool,
    pub writable: bool,
    pub executable: bool,
}

/// Opaque, inert hierarchy prepared before kernel mapping publication.
pub struct PreparedAddressSpace {
    #[cfg(any(CONFIG_ARCH_AARCH64, CONFIG_ARCH_RISCV64))]
    backend: crate::arch::user::PreparedUserAddressSpace,
}

#[derive(Clone, Copy)]
pub struct LocalIdentity {
    #[cfg(any(CONFIG_ARCH_AARCH64, CONFIG_ARCH_RISCV64))]
    backend: crate::arch::user::UserLocalIdentity,
}

#[derive(Clone, Copy)]
pub struct LocalRequest {
    #[cfg(any(CONFIG_ARCH_AARCH64, CONFIG_ARCH_RISCV64))]
    backend: crate::arch::user::UserLocalRequest,
}

/// CPU-affine proof that privileged access isolation was established while
/// the final native-entry interrupt mask remains owned by the caller.
#[must_use = "kernel-access preparation must be consumed by native activation"]
pub struct PreparedKernelAccess<'mask> {
    cpu: CpuIndex,
    _mask: &'mask InterruptMaskGuard<crate::hal::irq::LocalMask>,
}

impl PreparedAddressSpace {
    pub fn local_identity(&self) -> LocalIdentity {
        #[cfg(any(CONFIG_ARCH_AARCH64, CONFIG_ARCH_RISCV64))]
        {
            LocalIdentity {
                backend: self.backend.local_identity(),
            }
        }
        #[cfg(not(any(CONFIG_ARCH_AARCH64, CONFIG_ARCH_RISCV64)))]
        {
            LocalIdentity {}
        }
    }

    /// Encodes a root replacement for immediate acknowledged delivery.
    ///
    /// # Safety
    ///
    /// The prepared root and its identifier owner must remain retained until
    /// every target has acknowledged servicing the returned request.
    pub unsafe fn replace_request(&self) -> LocalRequest {
        #[cfg(any(CONFIG_ARCH_AARCH64, CONFIG_ARCH_RISCV64))]
        {
            LocalRequest {
                backend: self
                    .backend
                    .local_request(crate::arch::user::UserLocalOperation::Replace),
            }
        }
        #[cfg(not(any(CONFIG_ARCH_AARCH64, CONFIG_ARCH_RISCV64)))]
        {
            LocalRequest {}
        }
    }

    /// Encodes a tagged invalidation for immediate acknowledged delivery.
    ///
    /// # Safety
    ///
    /// The prepared root and its identifier owner must remain retained until
    /// every target has acknowledged servicing the returned request.
    pub unsafe fn invalidate_request(&self) -> LocalRequest {
        #[cfg(any(CONFIG_ARCH_AARCH64, CONFIG_ARCH_RISCV64))]
        {
            LocalRequest {
                backend: self
                    .backend
                    .local_request(crate::arch::user::UserLocalOperation::Invalidate),
            }
        }
        #[cfg(not(any(CONFIG_ARCH_AARCH64, CONFIG_ARCH_RISCV64)))]
        {
            LocalRequest {}
        }
    }
}

/// Applies a value-only request retained by the acknowledged RPC mailbox.
///
/// # Safety
///
/// The hierarchy and translation identifier named by the request must remain
/// retained until this CPU acknowledges completion. Execution admission must
/// remain closed for the requested transition, and the caller must serialize
/// the operation on this CPU with local translation activation.
pub unsafe fn service_local_request(request: LocalRequest) -> bool {
    #[cfg(any(CONFIG_ARCH_AARCH64, CONFIG_ARCH_RISCV64))]
    {
        // SAFETY: The closed kernel RPC keeps the root/ID owner alive until
        // this target acknowledges and closes address-space admission.
        // SAFETY: The caller retains the hierarchy and identifier until this
        // target acknowledges completion.
        unsafe { crate::arch::user::service_user_local_request(request.backend) }.is_ok()
    }
    #[cfg(not(any(CONFIG_ARCH_AARCH64, CONFIG_ARCH_RISCV64)))]
    {
        let _ = request;
        false
    }
}

/// CPU-local activation retained until the scheduler detaches this root.
///
/// The owner borrow keeps translation storage alive. CPU affinity is enforced
/// by the unsafe activation contract: every actual context switch must consume
/// this token on its original CPU before publishing the outgoing Thread.
#[must_use = "active native translation must be explicitly left before a context switch"]
pub struct ActiveAddressSpace<'owner> {
    cpu: CpuIndex,
    #[cfg(any(CONFIG_ARCH_AARCH64, CONFIG_ARCH_RISCV64))]
    backend: Option<crate::arch::user::UserLocalActivation>,
    _owner: PhantomData<&'owner dyn hyper::hal::user::UserTranslationOwner>,
    not_send_or_sync: PhantomData<*mut ()>,
}

impl ActiveAddressSpace<'_> {
    pub const fn cpu(&self) -> CpuIndex {
        self.cpu
    }
}

/// Builds a host stage-1 root from a complete immutable mapping snapshot.
///
/// # Safety
///
/// Allocator results must be new, zeroed, linearly mapped blocks of the
/// requested order and remain retained through acknowledged root retirement.
/// The generation identifies the retained software address-space owner. Hardware
/// ASIDs are acquired separately for each admitted activation interval.
pub unsafe fn prepare_host_address_space(
    generation: u64,
    mut enumerate: impl FnMut(&mut dyn FnMut(MappingPage)),
    allocator: &mut impl FnMut(usize) -> Option<PhysicalAddress>,
) -> Result<PreparedAddressSpace, AddressSpaceError> {
    #[cfg(any(CONFIG_ARCH_AARCH64, CONFIG_ARCH_RISCV64))]
    {
        let mut arch_enumerate = |visit: &mut dyn FnMut(crate::arch::user::UserMappingPage)| {
            enumerate(&mut |page| {
                visit(crate::arch::user::UserMappingPage {
                    address: page.address,
                    physical: page.physical,
                    readable: page.readable,
                    writable: page.writable,
                    executable: page.executable,
                });
            });
        };
        // SAFETY: The facade forwards the table ownership contract unchanged.
        let backend = unsafe {
            crate::arch::user::prepare_host_user_address_space(
                generation,
                &mut arch_enumerate,
                allocator,
            )
        }
        .map_err(AddressSpaceError::Backend)?;
        Ok(PreparedAddressSpace { backend })
    }
    #[cfg(not(any(CONFIG_ARCH_AARCH64, CONFIG_ARCH_RISCV64)))]
    {
        let _ = (generation, &mut enumerate, allocator);
        Err(AddressSpaceError::Unsupported)
    }
}

/// Installs a prepared root on the IRQ-masked current CPU.
///
/// # Safety
///
/// Kernel admission must remain closed against update cuts until this method
/// returns and the returned token is recorded active. The hierarchy and its
/// identifier must remain retained for the token's lifetime. Before enabling
/// interrupts, the caller must register this activation with a scheduler hook
/// that consumes it on this CPU before every actual context switch. A syscall
/// may retain it while executing, but a parked continuation must not retain it.
pub unsafe fn activate_local<'owner>(
    root: &PreparedAddressSpace,
    identifier: u16,
    identifier_epoch: u64,
    cpu: CpuIndex,
    _owner: &'owner dyn hyper::hal::user::UserTranslationOwner,
    kernel_access: &PreparedKernelAccess<'_>,
) -> Result<ActiveAddressSpace<'owner>, AddressSpaceError> {
    if kernel_access.cpu != cpu
        || crate::hal::cpu::current_index() != Some(cpu)
        || crate::hal::irq::local_enabled()
    {
        return Err(AddressSpaceError::InvalidCpu);
    }
    #[cfg(any(CONFIG_ARCH_AARCH64, CONFIG_ARCH_RISCV64))]
    {
        // SAFETY: The caller supplies the admission, switch-hook, and retention
        // proof required by the architecture-local transition.
        let backend = unsafe {
            crate::arch::user::activate_user_local(&root.backend, identifier, identifier_epoch)
        };
        Ok(ActiveAddressSpace {
            cpu,
            backend: Some(backend),
            _owner: PhantomData,
            not_send_or_sync: PhantomData,
        })
    }
    #[cfg(not(any(CONFIG_ARCH_AARCH64, CONFIG_ARCH_RISCV64)))]
    {
        let _ = (root, identifier, identifier_epoch, cpu, kernel_access);
        Err(AddressSpaceError::Unsupported)
    }
}

pub fn local_identity_is_active(identity: LocalIdentity) -> bool {
    #[cfg(any(CONFIG_ARCH_AARCH64, CONFIG_ARCH_RISCV64))]
    {
        crate::arch::user::user_local_identity_is_active(identity.backend)
    }
    #[cfg(not(any(CONFIG_ARCH_AARCH64, CONFIG_ARCH_RISCV64)))]
    {
        let _ = identity;
        false
    }
}

/// Restores the translation context which preceded native activation.
///
/// # Safety
///
/// The caller must execute IRQ-masked on `active.cpu()`, before the outgoing
/// Thread becomes available to another CPU.
pub unsafe fn deactivate_local(active: ActiveAddressSpace<'_>) -> Result<(), AddressSpaceError> {
    if crate::hal::cpu::current_index() != Some(active.cpu) || crate::hal::irq::local_enabled() {
        hyper::debug::invariant_failure("selected HAL user::deactivate_local CPU invariant");
    }
    #[cfg(any(CONFIG_ARCH_AARCH64, CONFIG_ARCH_RISCV64))]
    {
        let mut active = active;
        let Some(backend) = active.backend.take() else {
            hyper::debug::invariant_failure("selected HAL user::deactivate_local invariant");
        };
        // SAFETY: The caller proves this is the PE which installed the
        // consumed non-Send translation token.
        unsafe { crate::arch::user::deactivate_user_local(backend) };
        Ok(())
    }
    #[cfg(not(any(CONFIG_ARCH_AARCH64, CONFIG_ARCH_RISCV64)))]
    {
        let _ = active;
        Err(AddressSpaceError::Unsupported)
    }
}

impl Drop for ActiveAddressSpace<'_> {
    fn drop(&mut self) {
        #[cfg(any(CONFIG_ARCH_AARCH64, CONFIG_ARCH_RISCV64))]
        if self.backend.is_some() {
            // Hardware still references a borrowed root. Returning would end
            // the root borrow and permit use-after-free.
            hyper::debug::invariant_failure("selected HAL user::drop invariant");
        }
    }
}

/// Opaque native-user register owner attached to one kernel `UserThread`.
pub struct UserContext {
    #[cfg(any(CONFIG_ARCH_AARCH64, CONFIG_ARCH_RISCV64))]
    backend: crate::arch::user::UserContext,
}

impl UserContext {
    /// Copies a detached context. The caller must own it after machine-run
    /// publication and any resident FP state have been withdrawn.
    pub fn snapshot_registers(&self, output: &mut UserRegisterSnapshot) {
        *output = UserRegisterSnapshot::EMPTY;
        #[cfg(any(CONFIG_ARCH_AARCH64, CONFIG_ARCH_RISCV64))]
        self.backend.snapshot_registers(output);
    }

    pub fn set_entry_argument(&mut self, argument: u64) {
        #[cfg(any(CONFIG_ARCH_AARCH64, CONFIG_ARCH_RISCV64))]
        self.backend.set_entry_argument(argument);
        #[cfg(not(any(CONFIG_ARCH_AARCH64, CONFIG_ARCH_RISCV64)))]
        let _ = argument;
    }
}

/// Creates a stopped native-user context using the selected machine contract.
pub fn prepare_context(entry: u64, stack: u64, tls: u64) -> Result<UserContext, UserEntryError> {
    #[cfg(any(CONFIG_ARCH_AARCH64, CONFIG_ARCH_RISCV64))]
    {
        let address_limit = crate::arch::user::user_address_limit()
            .map_err(crate::arch::user::UserEntryError::from)
            .map_err(UserEntryError::Backend)?;
        let backend = crate::arch::user::UserContext::try_new(entry, stack, tls, address_limit)
            .map_err(|error| match error {
                crate::arch::user::UserEntryError::InvalidInitialContext => {
                    UserEntryError::InvalidContext
                }
                other => UserEntryError::Backend(other),
            })?;
        Ok(UserContext { backend })
    }
    #[cfg(not(any(CONFIG_ARCH_AARCH64, CONFIG_ARCH_RISCV64)))]
    {
        let _ = (entry, stack, tls);
        Err(UserEntryError::Unsupported)
    }
}

/// Asserts architecture protection required before privileged Rust can run
/// with a user-bearing translation root installed.
pub fn prepare_kernel_access<'mask>(
    mask: &'mask InterruptMaskGuard<crate::hal::irq::LocalMask>,
) -> Result<PreparedKernelAccess<'mask>, UserEntryError> {
    if crate::hal::irq::local_enabled() {
        return Err(UserEntryError::InterruptsEnabled);
    }
    let cpu = crate::hal::cpu::current_index().ok_or(UserEntryError::Unsupported)?;
    #[cfg(any(CONFIG_ARCH_AARCH64, CONFIG_ARCH_RISCV64))]
    {
        crate::arch::user::assert_kernel_access()
            .map_err(crate::arch::user::UserEntryError::from)
            .map_err(UserEntryError::Backend)?;
        Ok(PreparedKernelAccess { cpu, _mask: mask })
    }
    #[cfg(not(any(CONFIG_ARCH_AARCH64, CONFIG_ARCH_RISCV64)))]
    {
        let _ = (cpu, mask);
        Err(UserEntryError::Unsupported)
    }
}

/// Runs a Thread-owned Native context until a fault, interruption, or stop.
///
/// Syscalls execute on the current Thread's kernel stack and may block or
/// migrate. Translation ownership remains with the caller throughout the run.
///
/// # Safety
/// The caller retains the context and service at stable addresses until return.
/// Initial entry has IRQs masked and an admitted Native translation. Every
/// actual scheduler switch suspends Native machine state and leaves the local
/// translation; a resumed callback restores translation before returning to
/// userspace. No CPU-affine guard or borrow spans this call. Return is masked,
/// with architecture publication closed; the caller must then leave any active
/// translation before consuming the exit's exactly-once completion.
pub unsafe fn run_user<'context>(
    context: &'context mut UserContext,
    binding: UserRunBinding,
    service: &NativeCallService<'_>,
) -> Result<(UserExit<'context>, StoppedPublication), UserEntryError> {
    if crate::hal::irq::local_enabled() {
        return Err(UserEntryError::InterruptsEnabled);
    }
    #[cfg(any(CONFIG_ARCH_AARCH64, CONFIG_ARCH_RISCV64))]
    {
        // SAFETY: The caller supplies the Thread lifetime, scheduler suspension,
        // translation, and masked boundary obligations documented above.
        let exit = unsafe { crate::arch::user::run_user(&mut context.backend, binding, service) }
            .map_err(UserEntryError::Backend)?;
        let exit = UserExit::from_arch(exit);
        let stopped = StoppedPublication {
            binding: exit.binding(),
        };
        Ok((exit, stopped))
    }
    #[cfg(not(any(CONFIG_ARCH_AARCH64, CONFIG_ARCH_RISCV64)))]
    {
        let _ = (context, binding, service);
        Err(UserEntryError::Unsupported)
    }
}

/// Saves and withdraws Native CPU ownership before an actual Thread switch.
///
/// An interrupted syscall continues on its retained Thread stack after resume;
/// it reacquires Native CPU ownership only when returning to userspace.
///
/// # Safety
/// Local IRQs are masked and the outgoing Thread cannot execute concurrently.
/// The scheduler calls this after dropping its locks and before switching
/// stacks, then leaves the outgoing Native translation before another Thread
/// can execute. No raw frame or CPU-local borrow may span the switch.
pub unsafe fn suspend_current_user() {
    if crate::hal::irq::local_enabled() {
        hyper::debug::invariant_failure("Native suspension requires masked IRQs");
    }
    #[cfg(any(CONFIG_ARCH_AARCH64, CONFIG_ARCH_RISCV64))]
    crate::arch::user::suspend_current_user();
}

/// Non-forgeable proof that architecture publication for one native-user
/// generation is closed.
pub struct StoppedPublication {
    binding: UserRunBinding,
}

impl StoppedPublication {
    pub const fn binding(&self) -> UserRunBinding {
        self.binding
    }
}

#[cfg_attr(not(any(CONFIG_ARCH_AARCH64, CONFIG_ARCH_RISCV64)), allow(dead_code))]
pub enum UserExit<'context> {
    Fault {
        fault: UserFault,
        completion: ReturnCapability<'context>,
    },
    Interrupted {
        completion: ReturnCapability<'context>,
    },
}

#[cfg_attr(not(any(CONFIG_ARCH_AARCH64, CONFIG_ARCH_RISCV64)), allow(dead_code))]
impl<'context> UserExit<'context> {
    pub fn binding(&self) -> UserRunBinding {
        match self {
            Self::Fault { completion, .. } | Self::Interrupted { completion } => {
                completion.binding()
            }
        }
    }

    #[cfg(any(CONFIG_ARCH_AARCH64, CONFIG_ARCH_RISCV64))]
    fn from_arch(exit: crate::arch::user::UserExit<'context>) -> Self {
        match exit {
            crate::arch::user::UserExit::Fault { fault, completion } => Self::Fault {
                fault,
                completion: ReturnCapability {
                    backend: completion,
                },
            },
            crate::arch::user::UserExit::Interrupted { completion } => Self::Interrupted {
                completion: ReturnCapability {
                    backend: completion,
                },
            },
        }
    }
}

/// Exclusive authority to finish one stopped native-user context generation.
///
/// Completion methods update the saved context or mark it discarded; they do
/// not enter userspace. A binding mismatch retains ownership in
/// [`CompletionFailure`] for fail-stop handling, rather than allowing the
/// context to be reused or silently dropped.
#[must_use = "native-user return ownership must be resumed or discarded exactly once"]
pub struct ReturnCapability<'context> {
    #[cfg(any(CONFIG_ARCH_AARCH64, CONFIG_ARCH_RISCV64))]
    backend: crate::arch::user::UserReturnCapability<'context>,
    #[cfg(not(any(CONFIG_ARCH_AARCH64, CONFIG_ARCH_RISCV64)))]
    _context: PhantomData<&'context mut UserContext>,
}

#[cfg_attr(not(any(CONFIG_ARCH_AARCH64, CONFIG_ARCH_RISCV64)), allow(dead_code))]
impl<'context> ReturnCapability<'context> {
    pub fn binding(&self) -> UserRunBinding {
        #[cfg(any(CONFIG_ARCH_AARCH64, CONFIG_ARCH_RISCV64))]
        {
            self.backend.binding()
        }
        #[cfg(not(any(CONFIG_ARCH_AARCH64, CONFIG_ARCH_RISCV64)))]
        {
            hyper::debug::invariant_failure("selected HAL user::binding invariant")
        }
    }

    pub fn resume_execution(
        self,
        expected: UserRunBinding,
    ) -> Result<(), CompletionFailure<'context>> {
        #[cfg(any(CONFIG_ARCH_AARCH64, CONFIG_ARCH_RISCV64))]
        {
            self.backend
                .resume_execution(expected)
                .map_err(CompletionFailure::from_arch)
        }
        #[cfg(not(any(CONFIG_ARCH_AARCH64, CONFIG_ARCH_RISCV64)))]
        {
            let _ = (self, expected);
            hyper::debug::invariant_failure("selected HAL user::resume_execution invariant")
        }
    }

    pub fn discard(self, expected: UserRunBinding) -> Result<(), CompletionFailure<'context>> {
        #[cfg(any(CONFIG_ARCH_AARCH64, CONFIG_ARCH_RISCV64))]
        {
            self.backend
                .discard(expected)
                .map_err(CompletionFailure::from_arch)
        }
        #[cfg(not(any(CONFIG_ARCH_AARCH64, CONFIG_ARCH_RISCV64)))]
        {
            let _ = (self, expected);
            hyper::debug::invariant_failure("selected HAL user::discard invariant")
        }
    }
}

#[must_use = "completion failure retains native-user return ownership"]
pub struct CompletionFailure<'context> {
    error: UserEntryError,
    completion: ReturnCapability<'context>,
}

/// Uninhabited proof that completion-failure handling cannot return.
pub enum CompletionStopped {}

impl<'context> CompletionFailure<'context> {
    #[cfg(any(CONFIG_ARCH_AARCH64, CONFIG_ARCH_RISCV64))]
    fn from_arch(failure: crate::arch::user::UserCompletionFailure<'context>) -> Self {
        let (error, completion) = failure.into_parts();
        Self {
            error: UserEntryError::Backend(error),
            completion: ReturnCapability {
                backend: completion,
            },
        }
    }

    /// Retains an armed architecture return owner for an imminent fail-stop.
    ///
    /// A completion mismatch makes the context permanently unusable. The
    /// capability must therefore remain armed until the fatal path stops all
    /// execution; dropping it would trigger a second, less diagnostic halt.
    pub fn abandon_with(self, stop: impl FnOnce(UserEntryError) -> CompletionStopped) -> ! {
        let Self { error, completion } = self;
        #[cfg(any(CONFIG_ARCH_AARCH64, CONFIG_ARCH_RISCV64))]
        core::mem::forget(completion);
        #[cfg(not(any(CONFIG_ARCH_AARCH64, CONFIG_ARCH_RISCV64)))]
        let _ = completion;
        match stop(error) {}
    }
}

#[cfg(all(
    any(CONFIG_ARCH_AARCH64, CONFIG_ARCH_RISCV64),
    all(feature = "kernel-test", feature = "kernel-self-test")
))]
pub fn direct_native_call_count_for_test() -> usize {
    crate::arch::user::direct_native_call_count_for_test()
}

/// Diagnostic machine entries and actual syscall suspension boundaries.
#[cfg(all(
    any(CONFIG_ARCH_AARCH64, CONFIG_ARCH_RISCV64),
    all(feature = "kernel-test", feature = "kernel-self-test")
))]
pub fn native_run_counts_for_test() -> (usize, usize) {
    crate::arch::user::native_run_counts_for_test()
}

/// Whether the current syscall's original user frame still owns the FP bank.
#[cfg(all(
    CONFIG_ARCH_AARCH64,
    all(feature = "kernel-test", feature = "kernel-self-test")
))]
pub fn syscall_fp_resident_for_test(
    _mask: &InterruptMaskGuard<crate::hal::irq::LocalMask>,
) -> bool {
    if crate::hal::irq::local_enabled() {
        hyper::debug::invariant_failure("Native FP test query requires masked IRQs");
    }
    crate::arch::user::syscall_fp_resident_for_test()
}

/// Copies from machine-visible memory using the selected architecture seam.
///
/// # Safety
///
/// The ranges must be resident, valid for `length`, and non-overlapping. The
/// operation is non-faulting by construction of the retained page mapping.
/// Both ranges are Normal memory, never MMIO. This provides neither an atomic
/// snapshot nor a publication barrier; the owner synchronizes its protocol.
pub unsafe fn copy_from_exposed(
    source: *const u8,
    destination: *mut u8,
    length: usize,
) -> Result<(), ExposedCopyError> {
    #[cfg(any(CONFIG_ARCH_AARCH64, CONFIG_ARCH_RISCV64))]
    {
        // SAFETY: The caller provides the contract forwarded unchanged to the
        // selected external-memory implementation.
        unsafe { crate::arch::user::copy_from_exposed(source, destination, length) };
        Ok(())
    }
    #[cfg(not(any(CONFIG_ARCH_AARCH64, CONFIG_ARCH_RISCV64)))]
    {
        let _ = (source, destination, length);
        Err(ExposedCopyError)
    }
}

/// Copies to machine-visible memory using the selected architecture seam.
///
/// # Safety
///
/// The same requirements as [`copy_from_exposed`] apply with reversed data
/// direction.
pub unsafe fn copy_to_exposed(
    source: *const u8,
    destination: *mut u8,
    length: usize,
) -> Result<(), ExposedCopyError> {
    #[cfg(any(CONFIG_ARCH_AARCH64, CONFIG_ARCH_RISCV64))]
    {
        // SAFETY: The caller provides the forwarded external-memory contract.
        unsafe { crate::arch::user::copy_to_exposed(source, destination, length) };
        Ok(())
    }
    #[cfg(not(any(CONFIG_ARCH_AARCH64, CONFIG_ARCH_RISCV64)))]
    {
        let _ = (source, destination, length);
        Err(ExposedCopyError)
    }
}

/// Returns register-bank restores and boundary saves for lower-world runtime probes.
#[cfg(all(
    any(CONFIG_ARCH_AARCH64, CONFIG_ARCH_RISCV64),
    all(feature = "kernel-test", feature = "kernel-self-test")
))]
pub fn fp_state_counts_for_test() -> (usize, usize) {
    crate::arch::user::fp_state_counts_for_test()
}
