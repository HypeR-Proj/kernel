// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Bounded, capability-confined traversal validated against namespace epochs.

use super::instance::{DirectoryAnchor, Location, Mount, MountNamespace};
use super::objects::Error;
use super::resolve_state::{NameDescriptor, PendingComponent, PendingPath, StateError, Traversal};
use super::scratch::{ScratchBudget, ScratchString, ScratchVec};
use crate::fs::primitives::file_data::StorageBudget;
use hyper::mm::FallibleArc;
use {
    crate::fs::primitives::MAX_PATH_BYTES, crate::fs::primitives::Name,
    crate::fs::primitives::NodeKind, crate::fs::primitives::Path,
};

#[derive(Clone, Eq, PartialEq)]
struct Step {
    location: Location,
    name: Option<NameDescriptor>,
}
struct NormalizedPath {
    text: ScratchString,
    directory_required: bool,
}

pub(super) struct Resolved {
    pub(super) location: Option<Location>,
    pub(super) parent: Location,
    pub(super) name: ScratchString,
    pub(super) canonical: ScratchString,
    pub(super) epoch: u64,
}
impl Resolved {
    pub(super) fn existing(&self) -> Result<&Location, Error> {
        self.location.as_ref().ok_or(Error::Missing)
    }
}

pub(super) fn typed(
    namespace: &FallibleArc<MountNamespace>,
    root: &DirectoryAnchor,
    start: &DirectoryAnchor,
    value: &str,
    kind: NodeKind,
    follow: bool,
    budget: &ScratchBudget,
) -> Result<Location, Error> {
    let resolved = resolve(namespace, root, start, value, follow, false, budget)?;
    require_kind(resolved.existing()?, kind)
}

fn require_kind(location: &Location, kind: NodeKind) -> Result<Location, Error> {
    let actual = location
        .mount()
        .filesystem()
        .attributes(location.node())?
        .kind();
    if actual != kind {
        return Err(if kind == NodeKind::Directory {
            Error::NotDirectory
        } else if actual == NodeKind::Directory {
            Error::IsDirectory
        } else {
            Error::NotRegularFile
        });
    }
    Ok(location.clone())
}

/// Resolve a transaction's paths against exactly one topology. Filesystem
/// epochs are independently checked; the transaction revalidates publication.
pub(super) fn typed_in_view(
    namespace: &FallibleArc<MountNamespace>,
    view: &super::mounts::View,
    root: &DirectoryAnchor,
    start: &DirectoryAnchor,
    value: &str,
    kind: NodeKind,
    budget: &ScratchBudget,
) -> Result<Location, Error> {
    let normalized = normalize(value, budget)?;
    let root = root.location(view);
    let start = start.location(view);
    let mut epochs = Epochs {
        entries: ScratchVec::new(budget.clone()),
    };
    let result = walk(
        view,
        &root,
        &start,
        &normalized,
        WalkOptions {
            follow: false,
            missing: false,
        },
        &mut epochs,
        budget,
    );
    if !epochs.unchanged() || !namespace.mounts.is_current(view) {
        return Err(Error::Busy);
    }
    require_kind(result?.existing()?, kind)
}

struct Epochs {
    entries: ScratchVec<(FallibleArc<Mount>, u64)>,
}
impl Epochs {
    fn observe(&mut self, mount: &FallibleArc<Mount>) -> Result<u64, super::instance::Error> {
        if let Some((_, epoch)) = self
            .entries
            .iter()
            .find(|(seen, _)| seen.id() == mount.id())
        {
            return Ok(*epoch);
        }
        let epoch = mount.filesystem().epoch();
        if epoch & 1 != 0 {
            mount.filesystem().wait_for_namespace()?;
            return Err(super::instance::Error::Busy);
        }
        self.entries.push((mount.clone(), epoch))?;
        Ok(epoch)
    }
    fn unchanged(&self) -> bool {
        self.entries
            .iter()
            .all(|(mount, epoch)| mount.filesystem().epoch() == *epoch)
    }
}

pub(super) fn resolve(
    namespace: &FallibleArc<MountNamespace>,
    root: &DirectoryAnchor,
    start: &DirectoryAnchor,
    value: &str,
    follow: bool,
    missing: bool,
    budget: &ScratchBudget,
) -> Result<Resolved, Error> {
    let normalized = normalize(value, budget)?;
    for _ in 0..8 {
        let view = namespace.mounts.snapshot();
        let mut epochs = Epochs {
            entries: ScratchVec::new(budget.clone()),
        };
        let root = root.location(&view);
        let start = start.location(&view);
        let result = walk(
            &view,
            &root,
            &start,
            &normalized,
            WalkOptions { follow, missing },
            &mut epochs,
            budget,
        );
        // Validate both the mount snapshot and every filesystem whose edges
        // contributed to traversal, including ancestry reconstructed across '..'.
        if epochs.unchanged() && namespace.mounts.is_current(&view) {
            if matches!(result, Err(Error::Busy)) {
                continue;
            }
            return result;
        }
    }
    Err(Error::Busy)
}

struct WalkOptions {
    follow: bool,
    missing: bool,
}

#[inline(never)]
fn walk(
    view: &super::mounts::View,
    root: &Location,
    start: &Location,
    value: &NormalizedPath,
    options: WalkOptions,
    epochs: &mut Epochs,
    budget: &ScratchBudget,
) -> Result<Resolved, Error> {
    let mut prepared = prepare_walk(root, value, budget)?;
    if !value.text.starts_with('/') {
        restore_start(view, root, start, &mut prepared, epochs, budget)?;
    }
    walk_components(view, &mut prepared, options, epochs, budget)
}

struct PreparedWalk {
    // Drop ancestry before its retained name arena, just as the original locals.
    traversal: Traversal<Step, ScratchBudget>,
    pending: PendingPath<ScratchBudget>,
    _charge: crate::accounting::CommittedCharge,
}

// Construction's by-value temporary state must not stay below later RPC waits.
#[inline(never)]
fn prepare_walk(
    root: &Location,
    value: &NormalizedPath,
    budget: &ScratchBudget,
) -> Result<alloc::boxed::Box<PreparedWalk>, Error> {
    // This state survives every component RPC and IRQ-tail scheduler entry.
    // Charge one bounded request-owned allocation instead of retaining it on
    // each caller's kernel stack; all preparation failures release the charge.
    let charge = budget.reserve(core::mem::size_of::<PreparedWalk>())?;
    let path = Path::new(&value.text).map_err(|_| Error::InvalidPath)?;
    let mut pending = PendingPath::new(path, budget.clone()).map_err(map_state_error)?;
    if value.directory_required {
        pending.require_final_directory().map_err(map_state_error)?;
    }
    let traversal = Traversal::try_new(
        Step {
            location: root.clone(),
            name: None,
        },
        budget.clone(),
    )
    .map_err(map_state_error)?;
    let mut storage = hyper::mm::try_box_uninit::<PreparedWalk>().map_err(|_| Error::Allocation)?;
    let pointer = storage.as_mut_ptr();
    // SAFETY: storage exclusively owns correctly aligned PreparedWalk memory.
    // All fallible preparation has finished. Move each field exactly once into
    // its final allocation before exposing any reference; nothing can fail or
    // unwind between these writes and assume_init. Earlier failures drop the
    // still-local traversal, pending names and charge in their normal order.
    unsafe {
        core::ptr::addr_of_mut!((*pointer).traversal).write(traversal);
        core::ptr::addr_of_mut!((*pointer).pending).write(pending);
        core::ptr::addr_of_mut!((*pointer)._charge).write(charge);
        Ok(storage.assume_init())
    }
}

// Relative ancestry construction must retire before ordinary component lookup
// can wait on backend IPC. Borrow the prepared state, without copying it.
#[inline(never)]
fn restore_start(
    view: &super::mounts::View,
    root: &Location,
    start: &Location,
    prepared: &mut PreparedWalk,
    epochs: &mut Epochs,
    budget: &ScratchBudget,
) -> Result<(), Error> {
    let PreparedWalk {
        pending, traversal, ..
    } = prepared;
    for (location, name) in view.ancestry(root, start, budget, |mount| {
        epochs.observe(mount).map(|_| ())
    })? {
        traversal
            .descend(Step {
                location,
                name: Some(
                    pending
                        .retain_name(Name::new(&name).map_err(|_| Error::InvalidPath)?)
                        .map_err(map_state_error)?,
                ),
            })
            .map_err(map_state_error)?;
    }
    Ok(())
}

#[inline(never)]
fn walk_components(
    view: &super::mounts::View,
    prepared: &mut PreparedWalk,
    options: WalkOptions,
    epochs: &mut Epochs,
    budget: &ScratchBudget,
) -> Result<Resolved, Error> {
    let PreparedWalk {
        pending, traversal, ..
    } = prepared;
    let mut parent = traversal.current().location.clone();
    let mut final_name = ScratchString::new(budget.clone());
    while let Some(component) = pending.pop() {
        let current = &traversal.current().location;
        let epoch = epochs.observe(current.mount())?;
        if current
            .mount()
            .filesystem()
            .attributes(current.node())?
            .kind()
            != NodeKind::Directory
        {
            return Err(Error::NotDirectory);
        }
        match component {
            PendingComponent::Current => {}
            PendingComponent::Parent => traversal.parent(),
            PendingComponent::Name(descriptor) => {
                let name = pending.name(descriptor).ok_or(Error::InvalidPath)?;
                parent = current.clone();
                final_name = copy_string(name.as_str(), budget)?;
                let node = current
                    .mount()
                    .filesystem()
                    .lookup_child(current.node(), name)?;
                let Some(node) = node else {
                    if options.missing && pending.is_empty() {
                        return Ok(Resolved {
                            location: None,
                            parent,
                            name: final_name,
                            canonical: ScratchString::new(budget.clone()),
                            epoch,
                        });
                    }
                    return Err(Error::Missing);
                };
                let candidate = view.enter(Location::new(current.mount().clone(), node)?)?;
                epochs.observe(candidate.mount())?;
                let attributes = candidate
                    .mount()
                    .filesystem()
                    .attributes(candidate.node())?;
                if attributes.kind() == NodeKind::Symlink && (options.follow || !pending.is_empty())
                {
                    follow_link(pending, traversal, &candidate, attributes.size(), budget)?;
                } else {
                    traversal
                        .descend(Step {
                            location: candidate,
                            name: Some(descriptor),
                        })
                        .map_err(map_state_error)?;
                }
            }
        }
    }
    finish_walk(prepared, parent, final_name, epochs, budget)
}

// Canonical-name allocation and result construction follow component lookup;
// they must not reserve scratch below backend RPCs during traversal.
#[inline(never)]
fn finish_walk(
    prepared: &PreparedWalk,
    parent: Location,
    final_name: ScratchString,
    epochs: &mut Epochs,
    budget: &ScratchBudget,
) -> Result<Resolved, Error> {
    let PreparedWalk {
        pending, traversal, ..
    } = prepared;
    let mut length = 0usize;
    for step in traversal.visited() {
        let Some(descriptor) = step.name else {
            continue;
        };
        let name = pending.name(descriptor).ok_or(Error::InvalidPath)?;
        length = length
            .checked_add(name.as_bytes().len() + 1)
            .filter(|length| *length <= MAX_PATH_BYTES)
            .ok_or(Error::InvalidPath)?;
    }
    let mut canonical = ScratchString::new(budget.clone());
    canonical.try_reserve_exact(length.max(1))?;
    for step in traversal.visited() {
        let Some(descriptor) = step.name else {
            continue;
        };
        canonical.push('/')?;
        canonical.push_str(pending.name(descriptor).ok_or(Error::InvalidPath)?.as_str())?;
    }
    if canonical.is_empty() {
        canonical.push('/')?;
    }
    let epoch = epochs.observe(traversal.current().location.mount())?;
    Ok(Resolved {
        location: Some(traversal.current().location.clone()),
        parent,
        name: final_name,
        canonical,
        epoch,
    })
}

// Symlink-only buffers stay off ordinary component lookup stacks.
#[inline(never)]
fn follow_link(
    pending: &mut PendingPath<ScratchBudget>,
    traversal: &mut Traversal<Step, ScratchBudget>,
    candidate: &Location,
    size: u64,
    budget: &ScratchBudget,
) -> Result<(), Error> {
    let target = read_link(candidate, size, budget)?;
    let target = core::str::from_utf8(&target).map_err(|_| Error::InvalidPath)?;
    let target = normalize(target, budget)?;
    if pending
        .expand_symlink(&target.text, target.directory_required)
        .map_err(map_state_error)?
    {
        traversal.restart();
    }
    Ok(())
}

/// Repeated separators carry no name. A terminal separator is retained as a
/// typed directory requirement, without extending the pathname byte budget.
fn normalize(value: &str, budget: &ScratchBudget) -> Result<NormalizedPath, Error> {
    if value.is_empty() || value.len() > MAX_PATH_BYTES || value.as_bytes().contains(&0) {
        return Err(Error::InvalidPath);
    }
    let mut output = ScratchString::new(budget.clone());
    output.try_reserve_exact(value.len())?;
    if value.starts_with('/') {
        output.push('/')?;
    }
    for component in value.split('/').filter(|component| !component.is_empty()) {
        if !output.is_empty() && !output.ends_with('/') {
            output.push('/')?;
        }
        output.push_str(component)?;
    }
    Path::new(&output).map_err(|_| Error::InvalidPath)?;
    Ok(NormalizedPath {
        text: output,
        directory_required: value.ends_with('/'),
    })
}

pub(super) fn read_link(
    location: &Location,
    size: u64,
    budget: &ScratchBudget,
) -> Result<ScratchVec<u8>, Error> {
    let size = usize::try_from(size).map_err(|_| Error::InvalidPath)?;
    if size > MAX_PATH_BYTES {
        return Err(Error::InvalidPath);
    }
    let mut target = ScratchVec::new(budget.clone());
    target.try_reserve_exact(size)?;
    target.resize(size, 0)?;
    let actual = location
        .mount()
        .filesystem()
        .read_link(location.node(), &mut target)?;
    if actual != size {
        return Err(Error::Backend(super::instance::Error::InvalidBackendResult));
    }
    Ok(target)
}
fn copy_string(value: &str, budget: &ScratchBudget) -> Result<ScratchString, Error> {
    ScratchString::from_str(value, budget.clone()).map_err(Into::into)
}
const fn map_state_error(error: StateError<crate::accounting::ResourceError>) -> Error {
    match error {
        StateError::Allocation => Error::Allocation,
        StateError::InvalidPath => Error::InvalidPath,
        StateError::SymlinkLoop => Error::SymlinkLoop,
        StateError::Budget(error) => Error::Resource(error),
    }
}
