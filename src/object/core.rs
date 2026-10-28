// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Fallibly owned kernel objects and active-authority accounting.

use alloc::boxed::Box;
use core::any::{Any, TypeId};
use core::marker::PhantomData;
use core::mem::ManuallyDrop;
#[cfg(test)]
use core::num::NonZeroU32;
use core::sync::atomic::{AtomicUsize, Ordering, fence};

use hyper::mm::{AllocationError, DeferredArcDrop, FallibleArc, WeakFallibleArc, try_box};
use hyper::sync::{InterruptSpinLock, SpinLock};

pub(crate) use super::identity::Koid;
use super::identity::KoidReservation;
use super::{Rights, signals::SignalSource};

const RETIRED: usize = 1 << (usize::BITS - 1);
const ACTIVE_LIMIT: usize = RETIRED - 1;
const DEVICE_ASSIGNMENT_AUTHORITY_OBJECT_KIND: u32 =
    hyper::abi::native::HYPER_NATIVE_OBJECT_DEVICE_ASSIGNMENT_AUTHORITY;
const PHYSICAL_DEVICE_OBJECT_KIND: u32 = hyper::abi::native::HYPER_NATIVE_OBJECT_PHYSICAL_DEVICE;
const GUEST_MAILBOX_OBJECT_KIND: u32 = hyper::abi::native::HYPER_NATIVE_OBJECT_GUEST_MAILBOX;
const ROOT_TRANSITION_OBJECT_KIND: u32 = hyper::abi::native::HYPER_NATIVE_OBJECT_ROOT_TRANSITION;
const FILE_PROTECTION_OBJECT_KIND: u32 = hyper::abi::native::HYPER_NATIVE_OBJECT_FILE_PROTECTION;
const GUEST_MAPPING_OBJECT_KIND: u32 = hyper::abi::native::HYPER_NATIVE_OBJECT_GUEST_MAPPING;
const NATIVE_BLOCK_OBJECT_KIND: u32 = hyper::abi::native::HYPER_NATIVE_OBJECT_NATIVE_BLOCK;
const GUEST_NOTIFICATION_OBJECT_KIND: u32 =
    hyper::abi::native::HYPER_NATIVE_OBJECT_GUEST_NOTIFICATION;
const GUEST_INTERRUPT_OBJECT_KIND: u32 = hyper::abi::native::HYPER_NATIVE_OBJECT_GUEST_INTERRUPT;
const GUEST_MEMORY_OBJECT_KIND: u32 = hyper::abi::native::HYPER_NATIVE_OBJECT_GUEST_MEMORY;
const WAIT_SET_OBJECT_KIND: u32 = hyper::abi::native::HYPER_NATIVE_OBJECT_WAIT_SET;
const EVENT_OBJECT_KIND: u32 = hyper::abi::native::HYPER_NATIVE_OBJECT_EVENT;
const BYTE_CHANNEL_OBJECT_KIND: u32 = hyper::abi::native::HYPER_NATIVE_OBJECT_BYTE_CHANNEL;
const CAPABILITY_CHANNEL_OBJECT_KIND: u32 =
    hyper::abi::native::HYPER_NATIVE_OBJECT_CAPABILITY_CHANNEL;
const THREAD_OBJECT_KIND: u32 = hyper::abi::native::HYPER_NATIVE_OBJECT_THREAD;
const PROCESS_OBJECT_KIND: u32 = hyper::abi::native::HYPER_NATIVE_OBJECT_PROCESS;
const PROCESS_RETIREMENT_OBJECT_KIND: u32 =
    hyper::abi::native::HYPER_NATIVE_OBJECT_PROCESS_RETIREMENT;
const CRASH_VIEW_OBJECT_KIND: u32 = hyper::abi::native::HYPER_NATIVE_OBJECT_CRASH_VIEW;
const TASK_GROUP_OBJECT_KIND: u32 = hyper::abi::native::HYPER_NATIVE_OBJECT_TASK_GROUP;
const RESOURCE_DOMAIN_OBJECT_KIND: u32 = hyper::abi::native::HYPER_NATIVE_OBJECT_RESOURCE_DOMAIN;
const TASK_FACTORY_OBJECT_KIND: u32 = hyper::abi::native::HYPER_NATIVE_OBJECT_TASK_FACTORY;
const EXECUTABLE_AUTHORITY_OBJECT_KIND: u32 =
    hyper::abi::native::HYPER_NATIVE_OBJECT_EXECUTABLE_AUTHORITY;
const VMO_OBJECT_KIND: u32 = hyper::abi::native::HYPER_NATIVE_OBJECT_VMO;
const VMAR_OBJECT_KIND: u32 = hyper::abi::native::HYPER_NATIVE_OBJECT_VMAR;
const CONSOLE_OBJECT_KIND: u32 = hyper::abi::native::HYPER_NATIVE_OBJECT_CONSOLE;
const DIRECTORY_STREAM_OBJECT_KIND: u32 = hyper::abi::native::HYPER_NATIVE_OBJECT_DIRECTORY_STREAM;
const DIRECTORY_OBJECT_KIND: u32 = hyper::abi::native::HYPER_NATIVE_OBJECT_DIRECTORY;
const FILE_OBJECT_KIND: u32 = hyper::abi::native::HYPER_NATIVE_OBJECT_FILE;
const PROCESS_BUILDER_OBJECT_KIND: u32 = hyper::abi::native::HYPER_NATIVE_OBJECT_PROCESS_BUILDER;
const TASK_INSPECTOR_OBJECT_KIND: u32 = hyper::abi::native::HYPER_NATIVE_OBJECT_TASK_INSPECTOR;
const OBJECT_INSPECTOR_OBJECT_KIND: u32 = hyper::abi::native::HYPER_NATIVE_OBJECT_OBJECT_INSPECTOR;
const MEMORY_INSPECTOR_OBJECT_KIND: u32 = hyper::abi::native::HYPER_NATIVE_OBJECT_MEMORY_INSPECTOR;
const CPU_INSPECTOR_OBJECT_KIND: u32 = hyper::abi::native::HYPER_NATIVE_OBJECT_CPU_INSPECTOR;
const VIRTUAL_MACHINE_CREATION_AUTHORITY_OBJECT_KIND: u32 =
    hyper::abi::native::HYPER_NATIVE_OBJECT_VIRTUAL_MACHINE_CREATION_AUTHORITY;
const VIRTUAL_MACHINE_CREATION_LEASE_OBJECT_KIND: u32 =
    hyper::abi::native::HYPER_NATIVE_OBJECT_VIRTUAL_MACHINE_CREATION_LEASE;
const PENDING_VIRTUAL_MACHINE_OBJECT_KIND: u32 =
    hyper::abi::native::HYPER_NATIVE_OBJECT_PENDING_VIRTUAL_MACHINE;
const VIRTUAL_MACHINE_OBJECT_KIND: u32 = hyper::abi::native::HYPER_NATIVE_OBJECT_VIRTUAL_MACHINE;
const VIRTUAL_CPU_OBJECT_KIND: u32 = hyper::abi::native::HYPER_NATIVE_OBJECT_VIRTUAL_CPU;
const VIRTUAL_SERIAL_OBJECT_KIND: u32 = hyper::abi::native::HYPER_NATIVE_OBJECT_VIRTUAL_SERIAL;

const _: () = assert!(EVENT_OBJECT_KIND != 0);
const _: () = assert!(BYTE_CHANNEL_OBJECT_KIND != 0);
const _: () = assert!(CAPABILITY_CHANNEL_OBJECT_KIND != 0);
const _: () = assert!(THREAD_OBJECT_KIND != 0);
const _: () = assert!(PROCESS_OBJECT_KIND != 0);
const _: () = assert!(TASK_GROUP_OBJECT_KIND != 0);
const _: () = assert!(RESOURCE_DOMAIN_OBJECT_KIND != 0);
const _: () = assert!(TASK_FACTORY_OBJECT_KIND != 0);
const _: () = assert!(EXECUTABLE_AUTHORITY_OBJECT_KIND != 0);
const _: () = assert!(VMO_OBJECT_KIND != 0);
const _: () = assert!(VMAR_OBJECT_KIND != 0);
const _: () = assert!(CONSOLE_OBJECT_KIND != 0);
const _: () = assert!(PROCESS_BUILDER_OBJECT_KIND != 0);
const _: () = assert!(TASK_INSPECTOR_OBJECT_KIND != 0);
const _: () = assert!(OBJECT_INSPECTOR_OBJECT_KIND != 0);
const _: () = assert!(MEMORY_INSPECTOR_OBJECT_KIND != 0);
const _: () = assert!(CPU_INSPECTOR_OBJECT_KIND != 0);
const _: () = assert!(VIRTUAL_MACHINE_CREATION_AUTHORITY_OBJECT_KIND != 0);
const _: () = assert!(VIRTUAL_MACHINE_CREATION_LEASE_OBJECT_KIND != 0);
const _: () = assert!(PENDING_VIRTUAL_MACHINE_OBJECT_KIND != 0);
const _: () = assert!(VIRTUAL_MACHINE_OBJECT_KIND != 0);
const _: () = assert!(VIRTUAL_CPU_OBJECT_KIND != 0);
const _: () = assert!(VIRTUAL_SERIAL_OBJECT_KIND != 0);

#[cfg(not(test))]
type FinalReapQueueLock<T> = InterruptSpinLock<T, crate::hal::irq::LocalMask>;

#[cfg(test)]
struct TestReapQueueMask;

#[cfg(test)]
impl hyper::hal::interrupt::InterruptMask for TestReapQueueMask {
    type State = ();

    fn save_and_disable() -> Self::State {}

    fn restore(_state: Self::State) {}
}

#[cfg(test)]
type FinalReapQueueLock<T> = InterruptSpinLock<T, TestReapQueueMask>;

/// Stable object-kind identity used by the handle ABI.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct ObjectKind(u32);

impl ObjectKind {
    pub(crate) const EXECUTION_CONTEXT: Self =
        Self(hyper::abi::native::HYPER_NATIVE_OBJECT_EXECUTION_CONTEXT);
    pub(crate) const OBJECT_FACTORY: Self =
        Self(hyper::abi::native::HYPER_NATIVE_OBJECT_OBJECT_FACTORY);
    pub(crate) const SYSTEM_INFO: Self = Self(hyper::abi::native::HYPER_NATIVE_OBJECT_SYSTEM_INFO);
    pub(crate) const ROOT_TRANSITION: Self = Self(ROOT_TRANSITION_OBJECT_KIND);
    pub(crate) const FILE_PROTECTION: Self = Self(FILE_PROTECTION_OBJECT_KIND);
    pub(crate) const BACKEND_MEMORY_LEASE: Self =
        Self(hyper::abi::native::HYPER_NATIVE_OBJECT_BACKEND_MEMORY_LEASE);
    pub(crate) const DEVICE_ASSIGNMENT_AUTHORITY: Self =
        Self(DEVICE_ASSIGNMENT_AUTHORITY_OBJECT_KIND);
    pub(crate) const PHYSICAL_DEVICE: Self = Self(PHYSICAL_DEVICE_OBJECT_KIND);
    pub(crate) const GUEST_MAILBOX: Self = Self(GUEST_MAILBOX_OBJECT_KIND);
    pub(crate) const GUEST_INTERRUPT: Self = Self(GUEST_INTERRUPT_OBJECT_KIND);
    pub(crate) const GUEST_MAPPING: Self = Self(GUEST_MAPPING_OBJECT_KIND);
    pub(crate) const NATIVE_BLOCK: Self = Self(NATIVE_BLOCK_OBJECT_KIND);
    pub(crate) const BLOCK_QUEUE: Self = Self(hyper::abi::native::HYPER_NATIVE_OBJECT_BLOCK_QUEUE);
    pub(crate) const GUEST_NOTIFICATION: Self = Self(GUEST_NOTIFICATION_OBJECT_KIND);
    pub(crate) const GUEST_MEMORY: Self = Self(GUEST_MEMORY_OBJECT_KIND);
    pub(crate) const WAIT_SET: Self = Self(WAIT_SET_OBJECT_KIND);
    /// Native Event object kind declared by the generated ABI schema.
    pub(crate) const EVENT: Self = Self(EVENT_OBJECT_KIND);
    /// Buffered byte-stream endpoint kind declared by the generated ABI schema.
    pub(crate) const BYTE_CHANNEL: Self = Self(BYTE_CHANNEL_OBJECT_KIND);
    /// Synchronous capability-rendezvous endpoint kind.
    pub(crate) const CAPABILITY_CHANNEL: Self = Self(CAPABILITY_CHANNEL_OBJECT_KIND);
    /// Native user-thread kind declared by the generated ABI schema.
    pub(crate) const THREAD: Self = Self(THREAD_OBJECT_KIND);
    /// Native process-control kind declared by the generated ABI schema.
    pub(crate) const PROCESS: Self = Self(PROCESS_OBJECT_KIND);
    pub(crate) const PROCESS_RETIREMENT: Self = Self(PROCESS_RETIREMENT_OBJECT_KIND);
    pub(crate) const CRASH_VIEW: Self = Self(CRASH_VIEW_OBJECT_KIND);
    /// Grouped process-lifecycle kind declared by the generated ABI schema.
    pub(crate) const TASK_GROUP: Self = Self(TASK_GROUP_OBJECT_KIND);
    /// Hierarchical resource-accounting kind declared by the generated ABI schema.
    pub(crate) const RESOURCE_DOMAIN: Self = Self(RESOURCE_DOMAIN_OBJECT_KIND);
    /// Authority to construct task objects declared by the generated ABI schema.
    pub(crate) const TASK_FACTORY: Self = Self(TASK_FACTORY_OBJECT_KIND);
    /// Authority to derive immutable executable memory.
    pub(crate) const EXECUTABLE_AUTHORITY: Self = Self(EXECUTABLE_AUTHORITY_OBJECT_KIND);
    /// Native virtual-memory object kind declared by the generated ABI schema.
    pub(crate) const VMO: Self = Self(VMO_OBJECT_KIND);
    /// Native virtual-address-region kind declared by the generated ABI schema.
    pub(crate) const VMAR: Self = Self(VMAR_OBJECT_KIND);
    /// Capability-scoped host-console kind declared by the generated ABI schema.
    pub(crate) const CONSOLE: Self = Self(CONSOLE_OBJECT_KIND);
    /// Capability-relative filesystem directory authority.
    pub(crate) const DIRECTORY_STREAM: Self = Self(DIRECTORY_STREAM_OBJECT_KIND);
    pub(crate) const DIRECTORY: Self = Self(DIRECTORY_OBJECT_KIND);
    /// One opened filesystem file.
    pub(crate) const FILE: Self = Self(FILE_OBJECT_KIND);
    /// Linear staged authority to construct one Native Process.
    pub(crate) const PROCESS_BUILDER: Self = Self(PROCESS_BUILDER_OBJECT_KIND);
    /// Capability-scoped task-observation authority.
    pub(crate) const TASK_INSPECTOR: Self = Self(TASK_INSPECTOR_OBJECT_KIND);
    /// Capability-scoped object-graph observation authority.
    pub(crate) const OBJECT_INSPECTOR: Self = Self(OBJECT_INSPECTOR_OBJECT_KIND);
    /// Capability-scoped physical-memory observation authority.
    pub(crate) const MEMORY_INSPECTOR: Self = Self(MEMORY_INSPECTOR_OBJECT_KIND);
    /// Capability-scoped scheduler CPU-time observation authority.
    pub(crate) const CPU_INSPECTOR: Self = Self(CPU_INSPECTOR_OBJECT_KIND);
    /// Root authority for deriving one-shot VM construction leases.
    pub(crate) const VIRTUAL_MACHINE_CREATION_AUTHORITY: Self =
        Self(VIRTUAL_MACHINE_CREATION_AUTHORITY_OBJECT_KIND);
    /// Resource-domain-bound, one-shot VM construction authority.
    pub(crate) const VIRTUAL_MACHINE_CREATION_LEASE: Self =
        Self(VIRTUAL_MACHINE_CREATION_LEASE_OBJECT_KIND);
    /// Unpublished VM construction transaction.
    pub(crate) const PENDING_VIRTUAL_MACHINE: Self = Self(PENDING_VIRTUAL_MACHINE_OBJECT_KIND);
    /// Installed virtual-machine lifecycle authority.
    pub(crate) const VIRTUAL_MACHINE: Self = Self(VIRTUAL_MACHINE_OBJECT_KIND);
    /// Installed virtual-CPU observation and execution authority.
    pub(crate) const VIRTUAL_CPU: Self = Self(VIRTUAL_CPU_OBJECT_KIND);
    /// Runtime-owned guest input and shared output registration.
    pub(crate) const VIRTUAL_SERIAL: Self = Self(VIRTUAL_SERIAL_OBJECT_KIND);

    /// Validates one userspace-supplied object-kind discriminator.
    ///
    /// `NONE` is deliberately rejected: capability receive contracts are
    /// typed and never use a wildcard object kind.
    pub(crate) const fn try_from_raw(raw: u32) -> Option<Self> {
        match raw {
            EVENT_OBJECT_KIND
            | BYTE_CHANNEL_OBJECT_KIND
            | CAPABILITY_CHANNEL_OBJECT_KIND
            | THREAD_OBJECT_KIND
            | PROCESS_OBJECT_KIND
            | PROCESS_RETIREMENT_OBJECT_KIND
            | CRASH_VIEW_OBJECT_KIND
            | TASK_GROUP_OBJECT_KIND
            | RESOURCE_DOMAIN_OBJECT_KIND
            | TASK_FACTORY_OBJECT_KIND
            | EXECUTABLE_AUTHORITY_OBJECT_KIND
            | VMO_OBJECT_KIND
            | VMAR_OBJECT_KIND
            | CONSOLE_OBJECT_KIND
            | DIRECTORY_STREAM_OBJECT_KIND
            | DIRECTORY_OBJECT_KIND
            | ROOT_TRANSITION_OBJECT_KIND
            | FILE_PROTECTION_OBJECT_KIND
            | FILE_OBJECT_KIND
            | PROCESS_BUILDER_OBJECT_KIND
            | TASK_INSPECTOR_OBJECT_KIND
            | OBJECT_INSPECTOR_OBJECT_KIND
            | MEMORY_INSPECTOR_OBJECT_KIND
            | CPU_INSPECTOR_OBJECT_KIND
            | VIRTUAL_MACHINE_CREATION_AUTHORITY_OBJECT_KIND
            | VIRTUAL_MACHINE_CREATION_LEASE_OBJECT_KIND
            | PENDING_VIRTUAL_MACHINE_OBJECT_KIND
            | VIRTUAL_MACHINE_OBJECT_KIND
            | VIRTUAL_CPU_OBJECT_KIND
            | VIRTUAL_SERIAL_OBJECT_KIND
            | DEVICE_ASSIGNMENT_AUTHORITY_OBJECT_KIND
            | PHYSICAL_DEVICE_OBJECT_KIND
            | GUEST_MAILBOX_OBJECT_KIND
            | GUEST_INTERRUPT_OBJECT_KIND
            | GUEST_MAPPING_OBJECT_KIND
            | hyper::abi::native::HYPER_NATIVE_OBJECT_BLOCK_QUEUE
            | NATIVE_BLOCK_OBJECT_KIND
            | GUEST_NOTIFICATION_OBJECT_KIND
            | GUEST_MEMORY_OBJECT_KIND
            | WAIT_SET_OBJECT_KIND
            | hyper::abi::native::HYPER_NATIVE_OBJECT_BACKEND_MEMORY_LEASE => Some(Self(raw)),
            _ => None,
        }
    }

    /// Constructs a synthetic kind for host-only mechanism tests.
    ///
    /// A production constructor is added here only with a corresponding
    /// generated ABI kind, keeping type/kind coherence in one module.
    #[cfg(test)]
    pub(crate) const fn for_test(value: NonZeroU32) -> Self {
        Self(value.get())
    }

    pub(crate) const fn get(self) -> u32 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ObjectCreationError {
    Allocation,
    KoidExhausted,
    RegistrationExhausted,
}

/// Immutable userspace-publication policy attached at object construction.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ExportPolicy {
    KernelOnly,
    User,
}

/// Audit classification for moving authority between process namespaces.
///
/// `Leaf` objects never retain another userspace capability and may therefore
/// be stored by buffered transports. `RendezvousOnly` objects can participate
/// in ownership graphs and therefore require either a direct namespace commit
/// or an explicitly type-audited, bounded startup container. `Never` is the
/// default-deny class for objects without a reviewed transfer contract.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TransferClass {
    Never,
    Leaf,
    RendezvousOnly,
}

/// Strong-reference classes exposed by authority-free diagnostics.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ObjectReferenceSnapshot {
    pub(crate) kernel_service: usize,
    pub(crate) vm_device_binding: usize,
    pub(crate) scheduler: usize,
    pub(crate) publication: usize,
    pub(crate) user_authority: usize,
    pub(crate) operation_pin: usize,
    pub(crate) diagnostic: usize,
    pub(crate) retirement: usize,
}

#[derive(Clone, Copy)]
pub(crate) enum ReferenceKind {
    KernelService,
    VmDeviceBinding,
    Scheduler,
    Publication,
    UserAuthority,
    OperationPin,
    Diagnostic,
    Retirement,
}

impl ReferenceKind {
    const COUNT: usize = 8;

    const fn index(self) -> usize {
        self as usize
    }
}

mod reference_class_private {
    pub(crate) trait Sealed {}
}

/// Compile-time classification for one direct kernel-object owner.
pub(crate) trait ReferenceClass: reference_class_private::Sealed {
    const KIND: ReferenceKind;
}

macro_rules! reference_classes {
    ($($name:ident => $kind:ident),+ $(,)?) => {
        $(
            pub(crate) enum $name {}
            impl reference_class_private::Sealed for $name {}
            impl ReferenceClass for $name {
                const KIND: ReferenceKind = ReferenceKind::$kind;
            }
        )+
    };
}

reference_classes! {
    KernelService => KernelService,
    VmDeviceBinding => VmDeviceBinding,
    Scheduler => Scheduler,
    Publication => Publication,
    UserAuthority => UserAuthority,
    OperationPin => OperationPin,
    Diagnostic => Diagnostic,
    Retirement => Retirement,
}

impl From<AllocationError> for ObjectCreationError {
    fn from(_: AllocationError) -> Self {
        Self::Allocation
    }
}

/// Sealing vocabulary shared only by kernel object implementations.
pub(crate) mod private {
    pub(crate) trait Sealed {}
    pub(crate) trait UserExportable: Sealed {}
}

/// Closed kernel-object payload contract.
///
/// Implementations live in kernel subsystems and must also implement the
/// crate-private sealing trait. `on_zero_active_handles` is the sole transition
/// callback: it must be infallible, nonblocking, and allocation-free. It must
/// detach teardown state before releasing payload locks; detached capabilities
/// may then be released because their own zero-active callbacks are appended
/// to the allocation-free retirement queue rather than invoked recursively.
/// The transition rejects future handle preparation but is not operation
/// quiescence: resolved internal references may still be executing on other
/// CPUs, and the callback must not invalidate state they can access.
pub(crate) trait KernelObject: private::Sealed + Any + Send + Sync {
    const KIND: ObjectKind;
    /// Type-wide ceiling for every instance of this object payload.
    const SUPPORTED_RIGHTS: Rights;
    const TRANSFER_CLASS: TransferClass = TransferClass::Never;

    /// Capture bounded local metadata after all registry/handle locks are released.
    /// This may take the object's own metadata lock, but must not walk unrelated
    /// objects, touch hardware, allocate, copy user memory, or retain live pointers.
    fn diagnostic_details(
        &self,
        _cursor: u64,
    ) -> Result<super::diagnostics::Details, super::diagnostics::DetailError> {
        Err(super::diagnostics::DetailError::Unsupported)
    }

    /// Rights supported by this particular immutable object instance.
    ///
    /// Most payloads use the type-wide ceiling. Sum types whose variants have
    /// different authority surfaces override this method. The returned set is
    /// captured in the object header at construction and must be a subset of
    /// `SUPPORTED_RIGHTS`.
    fn supported_rights(&self) -> Rights {
        Self::SUPPORTED_RIGHTS
    }

    /// Exposes level-state observation only for objects which support `WAIT`.
    ///
    /// Presence is an immutable property of the concrete object. The returned
    /// source may borrow mutable level state through its own synchronization,
    /// but repeated calls must not add or remove the capability. This accessor
    /// runs under a Process handle-table lock and must remain side-effect-free,
    /// allocation-free, and nonblocking.
    fn signal_source(&self) -> Option<SignalSource<'_>> {
        None
    }

    /// Completes the zero-active transition after releasing the handle-table lock.
    ///
    /// The final authority release is not restricted to syscall context.
    /// Implementations must not allocate, block, fault on user memory,
    /// or perform hardware retirement. Publish durable stop/close state and
    /// wake existing waiters; transfer final destruction to `ObjectRetirement`
    /// or the subsystem reaper. Resolved operation owners can still exist.
    fn on_zero_active_handles(&self, _retirement: &mut ObjectRetirement) {}
}

/// Closed marker for object payloads which may be published to userspace.
///
/// Kernel-only object types deliberately cannot construct `ObjectPublication`.
/// This makes the publication boundary a type property rather than a rights
/// convention which a future caller could accidentally bypass.
pub(crate) trait UserExportableObject: KernelObject + private::UserExportable {}

impl<T> UserExportableObject for T where T: KernelObject + private::UserExportable {}

trait ErasedKernelObject: Any + Send + Sync {
    fn diagnostic_details(
        &self,
        cursor: u64,
    ) -> Result<super::diagnostics::Details, super::diagnostics::DetailError>;
    fn as_any(&self) -> &dyn Any;
    fn signal_source(&self) -> Option<SignalSource<'_>>;
    fn on_zero_active_handles(&self, retirement: &mut ObjectRetirement);
}

impl<T: KernelObject> ErasedKernelObject for T {
    fn diagnostic_details(
        &self,
        cursor: u64,
    ) -> Result<super::diagnostics::Details, super::diagnostics::DetailError> {
        KernelObject::diagnostic_details(self, cursor)
    }
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn signal_source(&self) -> Option<SignalSource<'_>> {
        KernelObject::signal_source(self)
    }

    fn on_zero_active_handles(&self, retirement: &mut ObjectRetirement) {
        KernelObject::on_zero_active_handles(self, retirement);
    }
}

struct ObjectHeader {
    kind: ObjectKind,
    supported_rights: Rights,
    transfer_class: TransferClass,
    payload_type: TypeId,
    export_policy: ExportPolicy,
    active_handles: AtomicUsize,
    references: [AtomicUsize; ReferenceKind::COUNT],
}

struct SharedObject {
    // Declared first so directory metadata is detached before payload charges
    // are released by field destruction.
    #[cfg(not(test))]
    directory: super::directory::Membership,
    header: ObjectHeader,
    payload: Box<dyn ErasedKernelObject>,
    // The allocation itself supplies its retirement-queue node. The link is
    // accessed only while the capability retirement queue is locked, so a
    // final-handle transition never needs to allocate work storage.
    retirement_next: SpinLock<Option<ErasedKernelRef<Retirement>>>,
    // The allocation also supplies its final-reap queue node. The queue's
    // deferred owners keep every linked allocation initialized, and only the
    // final-reap queue lock may inspect or replace this link.
    final_reap_next: SpinLock<Option<DeferredArcDrop<SharedObject>>>,
    // Return the identity slot only after directory detachment and payload
    // destruction, including destructors which may create another object.
    identity: KoidReservation<'static>,
}

/// Private type-erased allocation owner used to implement typed references.
pub(super) struct ObjectRef {
    allocation: ManuallyDrop<FallibleArc<SharedObject>>,
    class: ReferenceKind,
}

/// Direct, compiler-typed kernel reference with no handle-table lookup.
pub(crate) struct KernelRef<T: KernelObject, C: ReferenceClass> {
    owner: ObjectRef,
    marker: PhantomData<fn() -> (T, C)>,
}

/// Type-erased direct owner used only by heterogeneous capability storage.
pub(crate) struct ErasedKernelRef<C: ReferenceClass> {
    owner: ObjectRef,
    marker: PhantomData<fn() -> C>,
}

/// Exportable service owner. Only this type can derive a publication token.
pub(crate) struct PublishableRef<T: UserExportableObject, C: ReferenceClass> {
    reference: KernelRef<T, C>,
}

/// One candidate for first userspace publication of an exportable object.
///
/// Multiple candidates may be prepared from the same `PublishableRef`, but the
/// object's irreversible unpublished-to-active transition admits exactly one.
pub(crate) struct ObjectPublication<T: UserExportableObject> {
    reference: KernelRef<T, Publication>,
}

/// Linear ownership of exactly one contribution to the active-handle count.
///
/// Construction and duplication update the count before this value exists.
/// Consuming release updates it exactly once before relinquishing the matching
/// `UserAuthority` reference, so safe callers cannot separate lifetime
/// ownership from active authority accounting.
pub(crate) struct ActiveHandleOwner {
    object: Option<ErasedKernelRef<UserAuthority>>,
}

/// Non-owning directory reference to one object allocation.
///
/// This type deliberately exposes only upgrade and liveness observation. A
/// diagnostic registry must never become an authority or extend payload life.
pub(crate) struct WeakObjectRef(WeakFallibleArc<SharedObject>);

/// Active-handle state captured at one diagnostic observation point.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ObjectHandleState {
    Unpublished,
    Active(usize),
    Retired,
}

/// Authority-free object metadata suitable for debug and future ABI encoding.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ObjectSnapshot {
    pub(crate) koid: Koid,
    pub(crate) kind: ObjectKind,
    pub(crate) supported_rights: Rights,
    pub(crate) handles: ObjectHandleState,
    pub(crate) export_policy: ExportPolicy,
    pub(crate) references: ObjectReferenceSnapshot,
    /// Strong internal owners sampled independently from the class counters.
    ///
    /// Concurrent reference traffic makes this diagnostic snapshot weakly
    /// consistent: consumers must not require the class sum to equal this
    /// value. The temporary diagnostic owner is included for directory scans.
    pub(crate) strong_references: usize,
}

struct FinalReapQueue {
    head: Option<DeferredArcDrop<SharedObject>>,
    // Address of the final node. The head and embedded owner chain keep this
    // allocation live; an address avoids adding another owning reference.
    tail_address: Option<usize>,
}

impl FinalReapQueue {
    const fn new() -> Self {
        Self {
            head: None,
            tail_address: None,
        }
    }

    fn push(&mut self, object: DeferredArcDrop<SharedObject>) {
        let address = core::ptr::from_ref::<SharedObject>(&object).expose_provenance();
        if let Some(tail_address) = self.tail_address {
            // SAFETY: `tail_address` was exposed from the prior final node.
            // That node remains initialized in the owner chain rooted at
            // `head`, and the queue lock excludes both pop and another push.
            let tail =
                unsafe { &*core::ptr::with_exposed_provenance::<SharedObject>(tail_address) };
            tail.final_reap_next.with(|next| {
                if next.is_some() {
                    object_invariant_violation();
                }
                *next = Some(object);
            });
        } else {
            if self.head.is_some() {
                object_invariant_violation();
            }
            self.head = Some(object);
        }
        self.tail_address = Some(address);
    }

    fn pop(&mut self) -> Option<DeferredArcDrop<SharedObject>> {
        let head = self.head.take()?;
        self.head = head.final_reap_next.with(Option::take);
        if self.head.is_none() {
            self.tail_address = None;
        }
        Some(head)
    }

    const fn has_pending(&self) -> bool {
        self.head.is_some()
    }
}

static FINAL_REAP_QUEUE: FinalReapQueueLock<FinalReapQueue> =
    FinalReapQueueLock::new(FinalReapQueue::new());

impl ObjectRef {
    /// Heap bytes required by the erased owner and one concrete payload.
    pub(crate) const fn allocation_size<T: KernelObject>() -> Option<usize> {
        let object = match FallibleArc::<SharedObject>::allocation_size()
            .checked_add(core::mem::size_of::<T>())
        {
            Some(bytes) => bytes.checked_add(KoidReservation::allocation_size()),
            None => None,
        };
        #[cfg(not(test))]
        {
            match object {
                Some(bytes) => bytes.checked_add(super::directory::registration_size()),
                None => None,
            }
        }
        #[cfg(test)]
        {
            object
        }
    }

    /// Fallibly constructs an unpublished object with no active handles.
    fn try_new<T: KernelObject>(
        payload: T,
        export_policy: ExportPolicy,
        class: ReferenceKind,
    ) -> Result<Self, ObjectCreationError> {
        Self::try_new_boxed(try_box(payload)?, export_policy, class)
    }

    /// Adopts final payload storage without moving the object through the stack.
    fn try_new_boxed<T: KernelObject>(
        payload: Box<T>,
        export_policy: ExportPolicy,
        class: ReferenceKind,
    ) -> Result<Self, ObjectCreationError> {
        let supported_rights = payload.supported_rights();
        let signal_source = payload.signal_source();
        if !T::SUPPORTED_RIGHTS.contains(supported_rights)
            || supported_rights.contains(Rights::WAIT) != signal_source.is_some()
            || signal_source.is_some_and(SignalSource::has_empty_mask)
        {
            // Object definitions are closed kernel code. Publishing a WAIT
            // right without its mechanism, or an unreachable mechanism
            // without WAIT, would make authority depend on a hidden type test.
            object_invariant_violation();
        }
        let payload: Box<dyn ErasedKernelObject> = payload;
        let identity = super::identity::reserve()?;
        let object = SharedObject {
            #[cfg(not(test))]
            directory: super::directory::Membership::new(identity.koid()),
            header: ObjectHeader {
                kind: T::KIND,
                supported_rights,
                transfer_class: T::TRANSFER_CLASS,
                payload_type: TypeId::of::<T>(),
                export_policy,
                active_handles: AtomicUsize::new(0),
                references: core::array::from_fn(|index| {
                    AtomicUsize::new(usize::from(index == class.index()))
                }),
            },
            payload,
            retirement_next: SpinLock::new(None),
            final_reap_next: SpinLock::new(None),
            identity,
        };
        let object = Self {
            allocation: ManuallyDrop::new(FallibleArc::try_new(object)?),
            class,
        };
        #[cfg(not(test))]
        super::directory::register(&object)?;
        Ok(object)
    }

    pub(crate) fn koid(&self) -> Koid {
        self.allocation.identity.koid()
    }

    pub(crate) fn kind(&self) -> ObjectKind {
        self.allocation.header.kind
    }

    pub(crate) fn supported_rights(&self) -> Rights {
        self.allocation.header.supported_rights
    }

    pub(crate) fn transfer_class(&self) -> TransferClass {
        self.allocation.header.transfer_class
    }

    pub(super) fn downgrade(&self) -> WeakObjectRef {
        WeakObjectRef(self.allocation.downgrade())
    }

    #[cfg(not(test))]
    pub(super) fn publish_directory_membership(&self) {
        self.allocation.directory.publish();
    }

    pub(crate) fn snapshot(&self) -> ObjectSnapshot {
        let active = self
            .allocation
            .header
            .active_handles
            .load(Ordering::Acquire);
        let handles = match active {
            0 => ObjectHandleState::Unpublished,
            RETIRED => ObjectHandleState::Retired,
            count => ObjectHandleState::Active(count),
        };
        ObjectSnapshot {
            koid: self.koid(),
            kind: self.kind(),
            supported_rights: self.supported_rights(),
            handles,
            export_policy: self.allocation.header.export_policy,
            references: reference_snapshot(&self.allocation.header.references),
            strong_references: self.allocation.strong_count(),
        }
    }

    #[cfg(test)]
    pub(crate) fn active_handle_count(&self) -> usize {
        self.allocation
            .header
            .active_handles
            .load(Ordering::Relaxed)
            & ACTIVE_LIMIT
    }

    /// Reports whether the zero-active transition has latched.
    ///
    /// The latch prevents authority resurrection. It is not a teardown-complete
    /// signal: the winning thread invokes the object callback after latching it.
    #[cfg(test)]
    pub(crate) fn is_retired(&self) -> bool {
        self.allocation
            .header
            .active_handles
            .load(Ordering::Relaxed)
            == RETIRED
    }

    pub(crate) fn downcast_ref<T: KernelObject>(&self) -> Option<&T> {
        if self.allocation.header.payload_type != TypeId::of::<T>() {
            return None;
        }
        self.allocation.payload.as_any().downcast_ref::<T>()
    }

    /// Returns this object's type-erased signal observation capability.
    pub(crate) fn signal_source(&self) -> Option<SignalSource<'_>> {
        self.allocation.payload.signal_source()
    }

    pub(crate) fn acquire_initial_handle(&self) -> Result<(), ActiveHandleError> {
        self.allocation
            .header
            .active_handles
            .compare_exchange(0, 1, Ordering::Relaxed, Ordering::Relaxed)
            .map(|_| ())
            .map_err(|current| {
                if current == RETIRED {
                    ActiveHandleError::Retired
                } else {
                    ActiveHandleError::AlreadyActive
                }
            })
    }

    pub(crate) fn acquire_additional_handle(&self) -> Result<(), ActiveHandleError> {
        let active = &self.allocation.header.active_handles;
        let mut current = active.load(Ordering::Relaxed);
        loop {
            if current == RETIRED {
                return Err(ActiveHandleError::Retired);
            }
            if current == 0 {
                return Err(ActiveHandleError::AlreadyActive);
            }
            if current == ACTIVE_LIMIT {
                return Err(ActiveHandleError::CountExhausted);
            }
            match active.compare_exchange_weak(
                current,
                current + 1,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => return Ok(()),
                Err(observed) => current = observed,
            }
        }
    }

    /// Releases one owner and reports whether it won the zero-active cut.
    ///
    /// The winner must enqueue this object for callback completion. Returning
    /// the decision, instead of invoking object policy here, keeps capability
    /// destruction iterative even when a callback releases more handles.
    pub(crate) fn release_active_handle(&self) -> bool {
        let active = &self.allocation.header.active_handles;
        let mut current = active.load(Ordering::Relaxed);
        loop {
            if current == 0 || current == RETIRED {
                // PreparedHandle is the only constructor for an active owner.
                // Continuing would conceal a double release and make later
                // teardown decisions rely on a false active-handle count.
                object_invariant_violation();
            }
            let next = if current == 1 { RETIRED } else { current - 1 };
            match active.compare_exchange_weak(current, next, Ordering::Release, Ordering::Relaxed)
            {
                Ok(_) => {
                    if next == RETIRED {
                        // Pair with every releasing decrement whose active
                        // owner preceded this final one. Retirement queue
                        // publication follows this acquire before object
                        // policy can observe the completed owners.
                        fence(Ordering::Acquire);
                        return true;
                    }
                    return false;
                }
                Err(observed) => current = observed,
            }
        }
    }

    /// Appends one already-retired object behind this queue node.
    fn link_retirement_successor(&self, successor: ErasedKernelRef<Retirement>) {
        self.allocation.retirement_next.with(|next| {
            if next.is_some() {
                object_invariant_violation();
            }
            *next = Some(successor);
        });
    }

    /// Detaches the successor while the retirement queue is exclusively held.
    fn take_retirement_successor(&self) -> Option<ErasedKernelRef<Retirement>> {
        self.allocation.retirement_next.with(Option::take)
    }

    /// Completes object-specific policy after the zero-active cut.
    fn complete_zero_active_transition(&self, retirement: &mut ObjectRetirement) {
        if self
            .allocation
            .header
            .active_handles
            .load(Ordering::Acquire)
            != RETIRED
        {
            object_invariant_violation();
        }
        self.allocation.payload.on_zero_active_handles(retirement);
    }

    fn clone_as(&self, class: ReferenceKind) -> Self {
        let allocation = FallibleArc::clone(&self.allocation);
        retain_reference_class(&allocation.header.references[class.index()]);
        Self {
            allocation: ManuallyDrop::new(allocation),
            class,
        }
    }

    fn into_class(mut self, class: ReferenceKind) -> Self {
        if self.class.index() != class.index() {
            release_reference_class(&self.allocation.header.references[self.class.index()]);
            retain_reference_class(&self.allocation.header.references[class.index()]);
            self.class = class;
        }
        self
    }
}

/// Heap bytes required by one erased owner and concrete object payload.
pub(crate) const fn object_allocation_size<T: KernelObject>() -> Option<usize> {
    ObjectRef::allocation_size::<T>()
}

impl<T: KernelObject> KernelRef<T, KernelService> {
    /// Creates a service-owned object which can never become a userspace handle.
    pub(crate) fn try_new_service(payload: T) -> Result<Self, ObjectCreationError> {
        Ok(Self::from_owner(ObjectRef::try_new(
            payload,
            ExportPolicy::KernelOnly,
            ReferenceKind::KernelService,
        )?))
    }
}

impl<T: KernelObject> KernelRef<T, Scheduler> {
    /// Constructs a kernel-only object whose initial owner is the scheduler.
    pub(crate) fn try_new_scheduler(payload: T) -> Result<Self, ObjectCreationError> {
        Ok(Self::from_owner(ObjectRef::try_new(
            payload,
            ExportPolicy::KernelOnly,
            ReferenceKind::Scheduler,
        )?))
    }
}

impl<T: KernelObject> KernelRef<T, OperationPin> {
    /// Filesystem RPC retains transport identity, not userspace authority.
    /// Final client/server handle closure must still retire the channel.
    pub(crate) fn into_filesystem_transport(self) -> KernelRef<T, KernelService> {
        KernelRef::from_owner(self.owner.into_class(ReferenceKind::KernelService))
    }

    /// Commits a validated operation reference into persistent VM-device
    /// ownership after the corresponding userspace handle is consumed.
    ///
    /// This conversion is the sole bridge from transient handle resolution to
    /// a device binding which may outlive the source handle. Callers must
    /// complete the matching consume-on-success handle-table transaction.
    pub(crate) fn into_vm_device_binding(self) -> KernelRef<T, VmDeviceBinding> {
        KernelRef::from_owner(self.owner.into_class(ReferenceKind::VmDeviceBinding))
    }
}

impl<T: KernelObject, C: ReferenceClass> KernelRef<T, C> {
    #[cfg(test)]
    pub(crate) fn downgrade_for_test(&self) -> WeakObjectRef {
        self.owner.downgrade()
    }

    fn from_owner(owner: ObjectRef) -> Self {
        if owner.downcast_ref::<T>().is_none() || owner.class.index() != C::KIND.index() {
            object_invariant_violation();
        }
        Self {
            owner,
            marker: PhantomData,
        }
    }

    fn from_validated_owner(owner: ObjectRef) -> Self {
        if owner.class.index() != C::KIND.index() {
            object_invariant_violation();
        }
        Self {
            owner,
            marker: PhantomData,
        }
    }

    pub(crate) fn object(&self) -> &T {
        match self.owner.downcast_ref::<T>() {
            Some(object) => object,
            None => object_invariant_violation(),
        }
    }

    pub(crate) fn koid(&self) -> Koid {
        self.owner.koid()
    }

    pub(crate) fn snapshot(&self) -> ObjectSnapshot {
        self.owner.snapshot()
    }
}

impl<T: KernelObject, C: ReferenceClass> Clone for KernelRef<T, C> {
    fn clone(&self) -> Self {
        Self::from_validated_owner(self.owner.clone())
    }
}

impl<T: UserExportableObject, C: ReferenceClass> PublishableRef<T, C> {
    pub(crate) fn object(&self) -> &T {
        self.reference.object()
    }

    pub(crate) fn snapshot(&self) -> ObjectSnapshot {
        self.reference.snapshot()
    }

    pub(crate) fn koid(&self) -> Koid {
        self.reference.koid()
    }

    #[cfg(test)]
    pub(crate) fn active_handle_count(&self) -> usize {
        self.reference.owner.active_handle_count()
    }

    #[cfg(test)]
    pub(crate) fn is_retired(&self) -> bool {
        self.reference.owner.is_retired()
    }

    pub(crate) fn publication(&self) -> ObjectPublication<T> {
        ObjectPublication {
            reference: KernelRef::from_validated_owner(
                self.reference.owner.clone_as(ReferenceKind::Publication),
            ),
        }
    }
}

impl<T: UserExportableObject> PublishableRef<T, KernelService> {
    /// Transfers one service owner into the scheduler ownership class.
    pub(crate) fn into_scheduler(self) -> KernelRef<T, Scheduler> {
        KernelRef::from_owner(self.reference.owner.into_class(ReferenceKind::Scheduler))
    }
}

impl<T: UserExportableObject> PublishableRef<T, KernelService> {
    pub(crate) fn try_new(payload: T) -> Result<Self, ObjectCreationError> {
        Ok(Self {
            reference: KernelRef::from_owner(ObjectRef::try_new(
                payload,
                ExportPolicy::User,
                ReferenceKind::KernelService,
            )?),
        })
    }
}

impl<T: UserExportableObject, C: ReferenceClass> Clone for PublishableRef<T, C> {
    fn clone(&self) -> Self {
        Self {
            reference: self.reference.clone(),
        }
    }
}

impl<T: UserExportableObject> ObjectPublication<T> {
    pub(crate) fn object(&self) -> &T {
        self.reference.object()
    }

    pub(crate) fn koid(&self) -> Koid {
        self.reference.koid()
    }
    pub(crate) fn try_new(payload: T) -> Result<Self, ObjectCreationError> {
        Ok(Self {
            reference: KernelRef::from_owner(ObjectRef::try_new(
                payload,
                ExportPolicy::User,
                ReferenceKind::Publication,
            )?),
        })
    }

    /// Adopts an object constructed in its final allocation, preserving the
    /// payload address and charge without reallocating or moving the payload.
    pub(crate) fn try_new_boxed(payload: Box<T>) -> Result<Self, ObjectCreationError> {
        Ok(Self {
            reference: KernelRef::from_owner(ObjectRef::try_new_boxed(
                payload,
                ExportPolicy::User,
                ReferenceKind::Publication,
            )?),
        })
    }

    pub(crate) fn supported_rights(&self) -> Rights {
        self.reference.owner.supported_rights()
    }

    /// Irreversibly activates the first userspace handle owner.
    pub(crate) fn activate(self) -> Result<ActiveHandleOwner, ActiveHandleError> {
        let owner = self.reference.owner;
        if owner.allocation.header.export_policy != ExportPolicy::User {
            return Err(ActiveHandleError::NotExportable);
        }
        owner.acquire_initial_handle()?;
        Ok(ActiveHandleOwner {
            object: Some(ErasedKernelRef {
                owner: owner.into_class(ReferenceKind::UserAuthority),
                marker: PhantomData,
            }),
        })
    }
}

impl<C: ReferenceClass> ErasedKernelRef<C> {
    pub(crate) fn koid(&self) -> Koid {
        self.owner.koid()
    }

    pub(crate) fn kind(&self) -> ObjectKind {
        self.owner.kind()
    }

    pub(crate) fn snapshot(&self) -> ObjectSnapshot {
        self.owner.snapshot()
    }
}

impl ErasedKernelRef<Diagnostic> {
    pub(crate) fn details(
        &self,
        cursor: u64,
    ) -> Result<super::diagnostics::ObjectDetails, super::diagnostics::DetailError> {
        Ok(super::diagnostics::ObjectDetails {
            koid: self.koid(),
            kind: self.kind(),
            details: self.owner.allocation.payload.diagnostic_details(cursor)?,
        })
    }
}

impl ActiveHandleOwner {
    pub(crate) fn pin_diagnostic(&self) -> ErasedKernelRef<Diagnostic> {
        ErasedKernelRef {
            owner: self.object().owner.clone_as(ReferenceKind::Diagnostic),
            marker: PhantomData,
        }
    }
    fn object(&self) -> &ErasedKernelRef<UserAuthority> {
        match self.object.as_ref() {
            Some(object) => object,
            None => object_invariant_violation(),
        }
    }

    pub(crate) fn koid(&self) -> Koid {
        self.object().koid()
    }

    pub(crate) fn kind(&self) -> ObjectKind {
        self.object().kind()
    }

    pub(crate) fn transfer_class(&self) -> TransferClass {
        self.object().owner.transfer_class()
    }

    pub(crate) fn pin<T: KernelObject>(&self) -> Option<KernelRef<T, OperationPin>> {
        self.object().owner.downcast_ref::<T>()?;
        Some(KernelRef::from_validated_owner(
            self.object().owner.clone_as(ReferenceKind::OperationPin),
        ))
    }

    /// Pins one already-authorized invocation without extending active authority.
    pub(crate) fn pin_erased(&self) -> ErasedKernelRef<OperationPin> {
        ErasedKernelRef {
            owner: self.object().owner.clone_as(ReferenceKind::OperationPin),
            marker: PhantomData,
        }
    }

    pub(crate) fn pin_waitable(&self) -> Option<ErasedKernelRef<OperationPin>> {
        self.object().owner.signal_source()?;
        Some(ErasedKernelRef {
            owner: self.object().owner.clone_as(ReferenceKind::OperationPin),
            marker: PhantomData,
        })
    }

    /// Creates one additional active owner or leaves the count unchanged.
    pub(crate) fn try_duplicate(&self) -> Result<Self, ActiveHandleError> {
        self.object().owner.acquire_additional_handle()?;
        Ok(Self {
            object: Some(self.object().clone()),
        })
    }

    /// Releases exactly one active owner into the caller's iterative worklist.
    pub(crate) fn release_into(mut self, retirement: &mut ObjectRetirement) {
        let object = match self.object.take() {
            Some(object) => object,
            None => object_invariant_violation(),
        };
        if object.owner.release_active_handle() {
            retirement.enqueue(ErasedKernelRef {
                owner: object.owner.into_class(ReferenceKind::Retirement),
                marker: PhantomData,
            });
        }
    }
}

impl Drop for ActiveHandleOwner {
    fn drop(&mut self) {
        let Some(object) = self.object.take() else {
            return;
        };
        let mut retirement = ObjectRetirement::new();
        if object.owner.release_active_handle() {
            retirement.enqueue(ErasedKernelRef {
                owner: object.owner.into_class(ReferenceKind::Retirement),
                marker: PhantomData,
            });
        }
        retirement.drain();
    }
}

impl ErasedKernelRef<OperationPin> {
    /// Retains a typed operation owner after checking the sealed payload kind.
    pub(crate) fn pin<T: KernelObject>(&self) -> Option<KernelRef<T, OperationPin>> {
        self.owner.downcast_ref::<T>()?;
        Some(KernelRef::from_validated_owner(self.owner.clone()))
    }

    pub(crate) fn signal_source(&self) -> Option<SignalSource<'_>> {
        self.owner.signal_source()
    }
}

impl ErasedKernelRef<Retirement> {
    fn link_retirement_successor(&self, successor: Self) {
        self.owner.link_retirement_successor(successor);
    }

    fn take_retirement_successor(&self) -> Option<Self> {
        self.owner.take_retirement_successor()
    }

    fn complete_zero_active_transition(&self, retirement: &mut ObjectRetirement) {
        self.owner.complete_zero_active_transition(retirement);
    }
}

impl<C: ReferenceClass> Clone for ErasedKernelRef<C> {
    fn clone(&self) -> Self {
        Self {
            owner: self.owner.clone(),
            marker: PhantomData,
        }
    }
}

impl WeakObjectRef {
    pub(super) fn upgrade(&self) -> Option<ErasedKernelRef<Diagnostic>> {
        let allocation = self.0.upgrade()?;
        retain_reference_class(&allocation.header.references[ReferenceKind::Diagnostic.index()]);
        Some(ErasedKernelRef {
            owner: ObjectRef {
                allocation: ManuallyDrop::new(allocation),
                class: ReferenceKind::Diagnostic,
            },
            marker: PhantomData,
        })
    }

    pub(super) fn is_alive(&self) -> bool {
        self.0.is_alive()
    }
}

/// Allocation-free worklist for zero-active object callbacks.
///
/// A final handle contributes the already-allocated object as its own queue
/// node. Callbacks receive this same worklist, so releasing capability owners
/// can append successors without nesting another object callback on the Rust
/// stack. Each top-level handle release drains its complete callback closure
/// synchronously before returning.
pub(crate) struct ObjectRetirement {
    head: Option<RetirementLink>,
    tail: Option<RetirementLink>,
}

impl ObjectRetirement {
    pub(crate) const fn new() -> Self {
        Self {
            head: None,
            tail: None,
        }
    }

    fn enqueue(&mut self, object: ErasedKernelRef<Retirement>) {
        hyper::collections::linked_list::push_back(
            &mut self.head,
            &mut self.tail,
            RetirementLink(object),
        );
    }

    /// Consumes the outer worklist owner. A callback only borrows the worklist
    /// to append releases and cannot recursively drain it through that borrow.
    pub(crate) fn drain(mut self) {
        while let Some(object) = self.pop() {
            object.complete_zero_active_transition(&mut self);
        }
    }

    fn pop(&mut self) -> Option<ErasedKernelRef<Retirement>> {
        hyper::collections::linked_list::pop_front_with_tail(&mut self.head, &mut self.tail)
            .map(|link| link.0)
    }
}

#[derive(Clone)]
struct RetirementLink(ErasedKernelRef<Retirement>);

impl hyper::collections::linked_list::TailLink for RetirementLink {
    fn link_successor(&self, next: Self) {
        self.0.link_retirement_successor(next.0);
    }
    fn take_successor(&self) -> Option<Self> {
        self.0.take_retirement_successor().map(Self)
    }
}

impl Drop for ObjectRetirement {
    fn drop(&mut self) {
        if self.head.is_some() || self.tail.is_some() {
            object_invariant_violation();
        }
    }
}

impl Clone for ObjectRef {
    fn clone(&self) -> Self {
        self.clone_as(self.class)
    }
}

impl Drop for ObjectRef {
    fn drop(&mut self) {
        release_reference_class(&self.allocation.header.references[self.class.index()]);
        // SAFETY: `ObjectRef` stores its owner in `ManuallyDrop` solely so this
        // destructor can consume it. This method runs exactly once and never
        // accesses the field after taking it.
        let object = unsafe { ManuallyDrop::take(&mut self.allocation) };
        if let Some(object) = object.release_deferred() {
            validate_final_release(&object);
            FINAL_REAP_QUEUE.with(|queue| queue.push(object));
            #[cfg(not(test))]
            crate::reaper::request();
        }
    }
}

fn validate_final_release(object: &DeferredArcDrop<SharedObject>) {
    if object
        .header
        .references
        .iter()
        .any(|counter| counter.load(Ordering::Relaxed) != 0)
    {
        object_invariant_violation();
    }
    let active = object.header.active_handles.load(Ordering::Relaxed);
    if active != 0 && active != RETIRED {
        object_invariant_violation();
    }
}

fn reference_snapshot(references: &[AtomicUsize; ReferenceKind::COUNT]) -> ObjectReferenceSnapshot {
    ObjectReferenceSnapshot {
        kernel_service: references[ReferenceKind::KernelService.index()].load(Ordering::Relaxed),
        vm_device_binding: references[ReferenceKind::VmDeviceBinding.index()]
            .load(Ordering::Relaxed),
        scheduler: references[ReferenceKind::Scheduler.index()].load(Ordering::Relaxed),
        publication: references[ReferenceKind::Publication.index()].load(Ordering::Relaxed),
        user_authority: references[ReferenceKind::UserAuthority.index()].load(Ordering::Relaxed),
        operation_pin: references[ReferenceKind::OperationPin.index()].load(Ordering::Relaxed),
        diagnostic: references[ReferenceKind::Diagnostic.index()].load(Ordering::Relaxed),
        retirement: references[ReferenceKind::Retirement.index()].load(Ordering::Relaxed),
    }
}

fn retain_reference_class(counter: &AtomicUsize) {
    let mut current = counter.load(Ordering::Relaxed);
    loop {
        if current == usize::MAX {
            return;
        }
        match counter.compare_exchange_weak(
            current,
            current + 1,
            Ordering::Relaxed,
            Ordering::Relaxed,
        ) {
            Ok(_) => return,
            Err(observed) => current = observed,
        }
    }
}

fn release_reference_class(counter: &AtomicUsize) {
    let mut current = counter.load(Ordering::Relaxed);
    loop {
        if current == usize::MAX {
            return;
        }
        if current == 0 {
            object_invariant_violation();
        }
        match counter.compare_exchange_weak(
            current,
            current - 1,
            Ordering::Relaxed,
            Ordering::Relaxed,
        ) {
            Ok(_) => return,
            Err(observed) => current = observed,
        }
    }
}

/// Reports whether at least one zero-reference object awaits final drop.
///
/// This is a scheduling hint only: another reaper may consume the work before
/// the caller acts, and a producer may enqueue immediately after `false`.
#[cfg(not(test))]
pub(crate) fn final_reap_pending(_access: &crate::reaper::ReaperAccess) -> bool {
    FINAL_REAP_QUEUE.with(|queue| queue.has_pending())
}

#[cfg(test)]
pub(crate) fn final_reap_pending() -> bool {
    FINAL_REAP_QUEUE.with(|queue| queue.has_pending())
}

/// Performs one final object drop outside the reap-queue lock.
///
/// Object-specific destructors may acquire directory or subsystem locks and
/// may release further object references. Detaching the owner first therefore keeps
/// queue lock ordering acyclic and turns cascading final drops into new queue
/// entries rather than recursive destruction.
#[cfg(not(test))]
pub(crate) fn reap_one_final_object(_access: &mut crate::reaper::ReaperAccess) -> bool {
    let object = FINAL_REAP_QUEUE.with(FinalReapQueue::pop);
    let Some(object) = object else {
        return false;
    };
    drop(object);
    true
}

#[cfg(test)]
pub(crate) fn reap_one_final_object() -> bool {
    let object = FINAL_REAP_QUEUE.with(FinalReapQueue::pop);
    let Some(object) = object else {
        return false;
    };
    drop(object);
    true
}

/// Reaps at most `limit` objects without allocating.
#[cfg(test)]
pub(crate) fn reap_final_objects(limit: usize) -> usize {
    let mut reaped = 0usize;
    while reaped < limit && reap_one_final_object() {
        reaped += 1;
    }
    reaped
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ActiveHandleError {
    NotExportable,
    Retired,
    AlreadyActive,
    CountExhausted,
}

#[cold]
fn object_invariant_violation() -> ! {
    hyper::debug::invariant_failure("object lifecycle invariant")
}
