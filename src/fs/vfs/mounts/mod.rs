// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Immutable mount-table snapshots; publication never holds a lock across I/O.

use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use hyper::mm::FallibleArc;
use hyper::sync::SpinLock;

use super::instance::{Error, FilesystemInstance, Location, Mount, MountPin};
use super::scratch::{ScratchBudget, ScratchString, ScratchVec};
use {
    crate::accounting::CommittedCharge, crate::accounting::ResourceDomain,
    crate::accounting::ResourceDomainId,
};

mod root_switch;

const MAX_MOUNTS: usize = 128;
pub(super) const MAX_ROOT_MOVES: usize =
    hyper::abi::native::HYPER_NATIVE_FILESYSTEM_SWITCH_ROOT_MAX_MOVES as usize;

#[derive(Clone)]
struct Attachment {
    covered: Location,
    mounted: FallibleArc<Mount>,
    _pin: FallibleArc<AttachmentPin>,
}

struct AttachmentPin {
    _charge: CommittedCharge,
    _pin: MountPin,
}

impl Attachment {
    fn prepare(
        covered: Location,
        mounted: FallibleArc<Mount>,
        sponsor: &ResourceDomain,
    ) -> Result<Self, Error> {
        let charge = super::instance::allocation_charge::<AttachmentPin>(sponsor)?;
        let pin = covered.mount().filesystem().pin_mount(covered.node())?;
        Ok(Self {
            covered,
            mounted,
            _pin: FallibleArc::try_new(AttachmentPin {
                _charge: charge,
                _pin: pin,
            })?,
        })
    }
}

struct Topology {
    _charge: CommittedCharge,
    entries: ScratchVec<Attachment>,
}

#[derive(Clone)]
pub(super) struct View {
    root: Location,
    topology: Option<FallibleArc<Topology>>,
}

impl View {
    pub(super) fn root(&self) -> &Location {
        &self.root
    }

    fn entries(&self) -> &[Attachment] {
        match &self.topology {
            Some(topology) => &topology.entries,
            None => &[],
        }
    }

    fn same(&self, other: &Self) -> bool {
        if self.root != other.root {
            return false;
        }
        match (&self.topology, &other.topology) {
            (None, None) => true,
            (Some(left), Some(right)) => core::ptr::eq(&**left, &**right),
            _ => false,
        }
    }

    pub(super) fn enter(&self, location: Location) -> Result<Location, Error> {
        match self
            .entries()
            .iter()
            .find(|entry| entry.covered == location)
        {
            Some(entry) => Location::new(entry.mounted.clone(), entry.mounted.root()),
            None => Ok(location),
        }
    }

    pub(super) fn covers(&self, location: &Location) -> bool {
        self.entries()
            .iter()
            .any(|entry| entry.covered == *location)
    }

    /// Reconstruct each filesystem-local ancestry and the mount edges between
    /// them. The caller validates the epochs of every visited filesystem.
    pub(super) fn ancestry(
        &self,
        root: &Location,
        start: &Location,
        budget: &ScratchBudget,
        mut observe: impl FnMut(&FallibleArc<Mount>) -> Result<(), Error>,
    ) -> Result<ScratchVec<(Location, ScratchString)>, Error> {
        let mut segments = ScratchVec::new(budget.clone());
        let mut cursor = start.clone();
        for _ in 0..=MAX_MOUNTS {
            observe(cursor.mount())?;
            if cursor.mount().id() == root.mount().id() {
                let local =
                    cursor
                        .mount()
                        .filesystem()
                        .ancestry(root.node(), cursor.node(), budget)?;
                segments.push((cursor.mount().clone(), local))?;
                let mut result: ScratchVec<(Location, ScratchString)> =
                    ScratchVec::new(budget.clone());
                for (index, (mount, local)) in segments.into_iter().rev().enumerate() {
                    if index != 0 {
                        // Cross only the mount edge actually present in the
                        // retained start location's ancestry. Re-entering every
                        // covered node would redirect pre-mount cwd capabilities.
                        let attachment = self
                            .entries()
                            .iter()
                            .find(|entry| entry.mounted.id() == mount.id())
                            .ok_or(Error::InvalidInput)?;
                        let (location, _) = result.last_mut().ok_or(Error::InvalidInput)?;
                        if *location != attachment.covered {
                            return Err(Error::InvalidInput);
                        }
                        *location = Location::new(mount.clone(), mount.root())?;
                    }
                    for (node, name) in local {
                        result.push((Location::new(mount.clone(), node)?, name))?;
                    }
                }
                return Ok(result);
            }
            let attachment = self
                .entries()
                .iter()
                .find(|entry| entry.mounted.id() == cursor.mount().id())
                .ok_or(Error::InvalidInput)?;
            let local = cursor.mount().filesystem().ancestry(
                &cursor.mount().root(),
                cursor.node(),
                budget,
            )?;
            segments.push((cursor.mount().clone(), local))?;
            cursor = attachment.covered.clone();
        }
        Err(Error::InvalidInput)
    }
}

pub(super) struct MountTable {
    current: SpinLock<View>,
    // Read and written only while current is locked. Readers can keep using
    // immutable views, but topology edits wait for a reversible switch owner.
    transition_pending: AtomicBool,
    reclaim_cursor: AtomicUsize,
}

impl MountTable {
    pub(super) fn new(root: Location) -> Self {
        Self {
            current: SpinLock::new(View {
                root,
                topology: None,
            }),
            transition_pending: AtomicBool::new(false),
            reclaim_cursor: AtomicUsize::new(0),
        }
    }

    pub(super) fn snapshot(&self) -> View {
        self.current.with(|current| current.clone())
    }

    pub(super) fn is_current(&self, view: &View) -> bool {
        self.current.with(|current| current.same(view))
    }

    pub(super) fn reclaim_file_records(&self, mut limit: usize) -> usize {
        let view = self.snapshot();
        let count = view.entries().len() + 1;
        let start = self.reclaim_cursor.fetch_add(1, Ordering::Relaxed) % count;
        let mut reclaimed = 0;
        for offset in 0..count {
            if limit == 0 {
                break;
            }
            let index = (start + offset) % count;
            let filesystem = if index == 0 {
                view.root.mount().filesystem()
            } else {
                view.entries()[index - 1].mounted.filesystem()
            };
            let (inspected, removed) = filesystem.reclaim_file_records(limit);
            limit -= inspected;
            reclaimed += removed;
        }
        reclaimed
    }

    pub(super) fn reclaim_idle_records(&self, domain: Option<ResourceDomainId>) {
        // Cloning the immutable topology requires no allocation. Never hold
        // the mount-table lock while trying a filesystem's namespace mutex.
        let view = self.snapshot();
        view.root.mount().filesystem().reclaim_idle_records(domain);
        for attachment in view.entries() {
            attachment.mounted.filesystem().reclaim_idle_records(domain);
        }
    }

    /// Freeze admission before syncing; table locks never span backend IPC.
    /// The caller has dropped its resolution Location before entering here.
    pub(super) fn detach(
        &self,
        old: &View,
        mounted: &FallibleArc<Mount>,
        sponsor: &ResourceDomain,
    ) -> Result<(), Error> {
        if !old
            .entries()
            .iter()
            .any(|entry| entry.mounted.id() == mounted.id())
        {
            return Err(Error::InvalidInput);
        }
        let mut entries = ScratchVec::new(ScratchBudget::new(sponsor));
        entries.try_reserve_exact(old.entries().len())?;
        for entry in old.entries() {
            if entry.mounted.id() != mounted.id() {
                entries.push(entry.clone())?;
            }
        }
        let mut replacement = Some(Self::prepare_view(old.root.clone(), entries, sponsor)?);
        mounted.freeze()?;
        let result = mounted.unmount_with(|| {
            let retired = self.current.with(|current| {
                if self.transition_pending.load(Ordering::Relaxed) || !current.same(old) {
                    return Err(Error::Busy);
                }
                let prepared = replacement.take().ok_or(Error::InvalidBackendResult)?;
                Ok(core::mem::replace(current, prepared))
            })?;
            drop(retired);
            Ok(())
        });
        if result.is_err() {
            mounted.thaw();
        }
        result
    }

    /// The caller resolves against `old`; never publish a stale path after a
    /// concurrent root replacement or mount change.
    pub(super) fn attach(
        &self,
        old: &View,
        covered: Location,
        filesystem: FallibleArc<FilesystemInstance>,
        sponsor: &ResourceDomain,
    ) -> Result<(), Error> {
        let mounted = Mount::try_new(filesystem, Some(sponsor))?;
        self.attach_mount(old, covered, mounted, sponsor)
    }

    #[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
    pub(super) fn bind_ramfs(
        &self,
        old: &View,
        source: &Location,
        covered: Location,
        sponsor: &ResourceDomain,
    ) -> Result<(), Error> {
        let mounted = Mount::try_bind(source, sponsor)?;
        self.attach_mount(old, covered, mounted, sponsor)
    }

    fn attach_mount(
        &self,
        old: &View,
        covered: Location,
        mounted: FallibleArc<Mount>,
        sponsor: &ResourceDomain,
    ) -> Result<(), Error> {
        if covered == old.root
            || old.entries().len() >= MAX_MOUNTS
            || old.covers(&covered)
            || old.entries().iter().any(|entry| {
                covered.mount().id() == entry.mounted.id()
                    && covered.node().get() == entry.mounted.root().get()
            })
        {
            return Err(Error::Busy);
        }
        let attachment = Attachment::prepare(covered, mounted, sponsor)?;
        let mut entries = ScratchVec::new(ScratchBudget::new(sponsor));
        entries.try_reserve_exact(old.entries().len() + 1)?;
        for entry in old.entries() {
            entries.push(entry.clone())?;
        }
        entries.push(attachment)?;
        let replacement = Self::prepare_view(old.root.clone(), entries, sponsor)?;
        self.publish(old, replacement)
    }

    fn prepare_view(
        root: Location,
        entries: ScratchVec<Attachment>,
        sponsor: &ResourceDomain,
    ) -> Result<View, Error> {
        let charge = super::instance::allocation_charge::<Topology>(sponsor)?;
        Ok(View {
            root,
            topology: Some(FallibleArc::try_new(Topology {
                _charge: charge,
                entries,
            })?),
        })
    }

    fn publish(&self, old: &View, replacement: View) -> Result<(), Error> {
        let mut prepared = Some(replacement);
        let retired = self.replace(old, &mut prepared)?;
        drop(retired);
        Ok(())
    }

    fn replace(&self, old: &View, prepared: &mut Option<View>) -> Result<View, Error> {
        self.current.with(|current| {
            if self.transition_pending.load(Ordering::Relaxed) || !current.same(old) {
                return Err(Error::Busy);
            }
            let replacement = prepared.take().ok_or(Error::InvalidBackendResult)?;
            Ok(core::mem::replace(current, replacement))
        })
    }
}
