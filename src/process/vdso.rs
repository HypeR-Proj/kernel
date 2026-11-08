// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! One immutable ELF and one observation-only data page for Native loaders.

#[path = "../../vdso/state.rs"]
mod state;

use crate::accounting::ResourceDomain;
use crate::capability::{HandleError, HandleFlags, PreparedHandle, Rights};
use crate::mm::user_space::{ExecutableProvenance, MemoryObjectError, VmoObject};
use crate::object::ObjectPublication;
#[cfg(feature = "kernel-trace")]
use hyper::sync::atomic::Ordering;
use hyper::sync::{PublishedOnce, atomic::AtomicU64};

const IMAGE: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/vdso/image.so"));
const IMAGE_SIZE: u64 = 65536;
const READ: Rights = Rights::READ.union(Rights::MAP).union(Rights::DUPLICATE);
const CODE: Rights = READ.union(Rights::EXECUTE);

struct Pair {
    code: PreparedHandle,
    data: PreparedHandle,
    #[cfg(feature = "kernel-trace")]
    address: usize,
}
static PAIR: PublishedOnce<Pair> = PublishedOnce::new();

enum Error {
    Memory(MemoryObjectError),
    Handle(HandleError),
    Scheduler(crate::task::scheduler::Error),
    Layout,
}

impl core::fmt::Debug for Error {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Memory(error) => formatter.debug_tuple("Memory").field(error).finish(),
            Self::Handle(error) => formatter.debug_tuple("Handle").field(error).finish(),
            Self::Scheduler(error) => formatter.debug_tuple("Scheduler").field(error).finish(),
            Self::Layout => formatter.write_str("Layout"),
        }
    }
}

/// Best-effort boot preparation. Failure leaves loaders on the syscall path.
/// The root domain owns backing independently of any observing process.
pub(crate) fn initialize(domain: &ResourceDomain) {
    if let Err(error) =
        prepare(domain).and_then(|pair| PAIR.publish(pair).map_err(|_| Error::Layout))
    {
        crate::pr_warn!("HypeR: optional vDSO unavailable: {:?}", error);
    }
}

fn prepare(domain: &ResourceDomain) -> Result<Pair, Error> {
    if IMAGE.len() > IMAGE_SIZE as usize {
        return Err(Error::Layout);
    }
    let size = (IMAGE.len() as u64).div_ceil(4096) * 4096;
    let code = VmoObject::try_new_writable(size, domain).map_err(Error::Memory)?;
    code.write(0, IMAGE).map_err(Error::Memory)?;
    let storage = code.writable().ok_or(Error::Layout)?;
    let prepared = storage
        .prepare_executable_snapshot(
            &ExecutableProvenance::for_native_image_loader(),
            crate::mm::user_space::DomainAccount::new(domain.clone()),
        )
        .map_err(|error| Error::Memory(MemoryObjectError::Vmo(error)))?;
    let executable =
        crate::mm::user_space::publish_snapshot(prepared).map_err(|error| match error {
            crate::mm::user_space::PublicationError::Vmo(error) => {
                Error::Memory(MemoryObjectError::Vmo(error))
            }
            crate::mm::user_space::PublicationError::Scheduler(error) => Error::Scheduler(error),
        })?;
    let code = VmoObject::from_executable(executable, domain).map_err(Error::Memory)?;
    let data = VmoObject::try_new_writable(4096, domain).map_err(Error::Memory)?;
    let storage = data.writable_clone().ok_or(Error::Layout)?;
    storage
        .populate(0, 4096)
        .map_err(|e| Error::Memory(MemoryObjectError::Vmo(e.cause)))?;
    let physical = storage
        .resident_physical_page(0)
        .map_err(MemoryObjectError::Vmo)
        .map_err(Error::Memory)?;
    let address = crate::mm::memory::linear_address(physical.get()).ok_or(Error::Layout)?;
    let (seconds, nanos, monotonic) = crate::time::vdso_realtime_anchor();
    let frequency = if cfg!(target_arch = "aarch64") {
        crate::time::counter_frequency_hz()
            .ok()
            .filter(|hz| *hz <= u32::MAX as u64)
            .unwrap_or(0)
    } else {
        0
    };
    let initial = state::State {
        frequency: AtomicU64::new(frequency),
        realtime_seconds: AtomicU64::new(seconds as u64),
        realtime_nanos: AtomicU64::new(u64::from(nanos)),
        realtime_monotonic: AtomicU64::new(monotonic),
        ipc_active: AtomicU64::new(0),
    };
    // SAFETY: This freshly allocated, resident page is naturally aligned and
    // not yet published. Its only mutable access after publication is atomic.
    unsafe {
        (address as *mut state::State).write(initial);
    }
    let data = VmoObject::from_observation(storage, domain).map_err(Error::Memory)?;
    Ok(Pair {
        code: handle(code, CODE)?,
        data: handle(data, READ)?,
        #[cfg(feature = "kernel-trace")]
        address,
    })
}

fn handle(object: VmoObject, rights: Rights) -> Result<PreparedHandle, Error> {
    let publication = ObjectPublication::try_new(object)
        .map_err(MemoryObjectError::Object)
        .map_err(Error::Memory)?;
    PreparedHandle::try_from_new_object(publication, rights, HandleFlags::NONE)
        .map_err(Error::Handle)
}

pub(crate) fn available() -> bool {
    PAIR.get().is_some()
}

pub(crate) fn handles() -> Result<Option<[PreparedHandle; 2]>, HandleError> {
    PAIR.get()
        .map(|pair| {
            Ok([
                pair.code.try_duplicate(CODE)?,
                pair.data.try_duplicate(READ)?,
            ])
        })
        .transpose()
}

/// Called at the trace recorder's serialized admission transition, not on RPCs.
#[cfg(feature = "kernel-trace")]
pub(crate) fn trace_activity(categories: u8) {
    if let Some(pair) = PAIR.get() {
        // SAFETY: PAIR permanently retains the read-only VMO and its resident
        // page. No writable/snapshot capability is exported. All post-publication
        // updates use this AtomicU64; user observers perform atomic reads too.
        let state = unsafe { &*(pair.address as *const state::State) };
        state
            .ipc_active
            .store(u64::from(categories & 2 != 0), Ordering::Release);
    }
}
