// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! One transaction publishes a new root, relocated mounts and detached RAM roots.

use super::{Attachment, Error, FilesystemInstance, Location, Mount, MountTable, View};
use super::{FallibleArc, MAX_MOUNTS, MAX_ROOT_MOVES, ResourceDomain, ScratchBudget, ScratchVec};
use {crate::fs::vfs::ramfs::promotion::LockedParent, crate::fs::vfs::ramfs::promotion::Parent};

struct NamespaceLock {
    filesystem: FallibleArc<FilesystemInstance>,
    epoch: u64,
    edit: bool,
}

impl NamespaceLock {
    fn observe(
        locks: &mut ScratchVec<Self>,
        filesystem: &FallibleArc<FilesystemInstance>,
        edit: bool,
    ) -> Result<(), Error> {
        if let Some(existing) = locks
            .iter_mut()
            .find(|entry| entry.filesystem.id() == filesystem.id())
        {
            existing.edit |= edit;
            return Ok(());
        }
        let epoch = filesystem.epoch();
        if epoch & 1 != 0 {
            return Err(Error::Busy);
        }
        locks.push(Self {
            filesystem: filesystem.clone(),
            epoch,
            edit,
        })?;
        Ok(())
    }
}

impl MountTable {
    /// Prepare a reversible switch using mounted roots only. Unlike RAM subtree
    /// promotion this changes no filesystem dirents, so abort needs only the
    /// retained old view and cannot allocate or call a filesystem provider.
    pub(in crate::fs::vfs) fn prepare_root_transition(
        &self,
        old: &View,
        new_root: Location,
        put_old: Location,
        moves: &[(Location, Location)],
        sponsor: &ResourceDomain,
    ) -> Result<(View, u64), Error> {
        if new_root.mount().id() == old.root.mount().id()
            || !is_mounted_root(old, &new_root)
            || old.root.node().get() != old.root.mount().root().get()
            || moves.len() > MAX_ROOT_MOVES
        {
            return Err(Error::InvalidInput);
        }
        let epoch = new_root.mount().filesystem().epoch();
        if epoch & 1 != 0 {
            return Err(Error::Busy);
        }
        let budget = ScratchBudget::new(sponsor);
        let mut replacements = ScratchVec::new(budget.clone());
        replacements.try_reserve_exact(moves.len() + 1)?;
        Self::validate_target(old, &new_root, &put_old)?;
        replacements.push(Attachment::prepare(
            put_old,
            old.root.mount().clone(),
            sponsor,
        )?)?;
        for (index, (source, target)) in moves.iter().enumerate() {
            Self::validate_target(old, &new_root, target)?;
            if !is_mounted_root(old, source)
                || source.mount().id() == new_root.mount().id()
                || replacements.iter().any(|entry| entry.covered == *target)
                || moves[..index]
                    .iter()
                    .any(|(previous, _)| previous == source)
            {
                return Err(Error::InvalidInput);
            }
            replacements.push(Attachment::prepare(
                target.clone(),
                source.mount().clone(),
                sponsor,
            )?)?;
        }
        let mut entries = ScratchVec::new(budget);
        entries.try_reserve_exact(old.entries().len() + 1)?;
        for entry in old.entries() {
            if entry.mounted.id() != new_root.mount().id()
                && !replacements
                    .iter()
                    .any(|next| next.mounted.id() == entry.mounted.id())
            {
                entries.push(entry.clone())?;
            }
        }
        for replacement in replacements {
            entries.push(replacement)?;
        }
        if entries.len() > MAX_MOUNTS {
            return Err(Error::Busy);
        }
        Self::validate_destinations(&new_root, &entries, sponsor)?;
        Ok((Self::prepare_view(new_root, entries, sponsor)?, epoch))
    }

    pub(in crate::fs::vfs) fn activate_root_transition(
        &self,
        old: &View,
        prepared: &mut Option<View>,
    ) -> Result<View, Error> {
        self.current.with(|current| {
            if self.transition_pending.load(super::Ordering::Relaxed) || !current.same(old) {
                return Err(Error::Busy);
            }
            let next = prepared.take().ok_or(Error::InvalidInput)?;
            self.transition_pending
                .store(true, super::Ordering::Relaxed);
            Ok(core::mem::replace(current, next))
        })
    }

    /// The active token is the sole topology writer until this call. Swapping
    /// the saved view restores every mount edge without allocation or I/O.
    pub(in crate::fs::vfs) fn finish_root_transition(&self, old: View, commit: bool) -> View {
        self.current.with(|current| {
            if !self.transition_pending.load(super::Ordering::Relaxed) {
                hyper::debug::invariant_failure("VFS root transition lost topology ownership");
            }
            self.transition_pending
                .store(false, super::Ordering::Relaxed);
            if commit {
                old
            } else {
                core::mem::replace(current, old)
            }
        })
    }

    pub(in crate::fs::vfs) fn switch_root(
        &self,
        old: &View,
        new_root: Location,
        put_old: Location,
        moves: &[(Location, Location)],
        sponsor: &ResourceDomain,
    ) -> Result<(), Error> {
        if new_root.mount().id() == old.root.mount().id()
            || !is_mounted_root(old, &new_root)
            || old.root.node().get() != old.root.mount().root().get()
            || moves.len() > MAX_ROOT_MOVES
        {
            return Err(Error::InvalidInput);
        }
        let budget = ScratchBudget::new(sponsor);
        let mut locks = ScratchVec::new(budget.clone());
        let mut parents = ScratchVec::new(budget.clone());
        NamespaceLock::observe(&mut locks, new_root.mount().filesystem_owner(), false)?;
        let mut replacements = ScratchVec::new(budget.clone());
        replacements.try_reserve_exact(moves.len() + 1)?;
        Self::validate_target(old, &new_root, &put_old)?;
        replacements.push(Attachment::prepare(
            put_old,
            old.root.mount().clone(),
            sponsor,
        )?)?;
        for (index, (source, target)) in moves.iter().enumerate() {
            Self::validate_target(old, &new_root, target)?;
            if source.mount().id() == new_root.mount().id()
                || source == old.root()
                || replacements.iter().any(|entry| entry.covered == *target)
                || moves[..index]
                    .iter()
                    .any(|(previous, _)| previous == source)
            {
                return Err(Error::InvalidInput);
            }
            let mounted = if is_mounted_root(old, source) {
                source.mount().clone()
            } else {
                NamespaceLock::observe(&mut locks, source.mount().filesystem_owner(), true)?;
                validate_promotion(old, &new_root, source, moves, &budget)?;
                let mounted = Mount::try_bind(source, sponsor)?;
                Parent::prepare(&mut parents, source.node().ramfs()?, &budget)?;
                mounted
            };
            replacements.push(Attachment::prepare(target.clone(), mounted, sponsor)?)?;
        }
        let mut entries = ScratchVec::new(budget.clone());
        entries.try_reserve_exact(old.entries().len() + parents.len())?;
        for entry in old.entries() {
            if entry.mounted.id() != new_root.mount().id()
                && !replacements
                    .iter()
                    .any(|replacement| replacement.mounted.id() == entry.mounted.id())
            {
                entries.push(entry.clone())?;
            }
        }
        for replacement in replacements {
            entries.push(replacement)?;
        }
        if entries.len() > MAX_MOUNTS {
            return Err(Error::Busy);
        }
        Self::validate_destinations(&new_root, &entries, sponsor)?;
        let mut prepared = Some(Self::prepare_view(new_root, entries, sponsor)?);
        let mut locked_parents = ScratchVec::new(budget);
        locked_parents.try_reserve_exact(parents.len())?;
        locks.sort_unstable_by_key(|entry| entry.filesystem.id().get());
        let retired = with_namespace_locks(&locks, || {
            self.publish_switch(old, &mut parents, locked_parents, &mut prepared)
        })?;
        // Removed dirents and the old view retire outside all namespace locks.
        drop(retired);
        Ok(())
    }

    // Keep the leaf transaction's scratch frame out of every recursive
    // namespace-lock frame. All sleeping locks precede the spinlock commit.
    #[inline(never)]
    fn publish_switch<'a>(
        &self,
        old: &View,
        parents: &'a mut [Parent],
        mut locked_parents: ScratchVec<LockedParent<'a>>,
        prepared: &mut Option<View>,
    ) -> Result<View, Error> {
        for parent in parents.iter_mut() {
            locked_parents.push(parent.lock()?)?;
        }
        let now = crate::time::realtime();
        self.current.with(|current| {
            if self.transition_pending.load(super::Ordering::Relaxed) || !current.same(old) {
                return Err(Error::Busy);
            }
            let replacement = prepared.take().ok_or(Error::InvalidBackendResult)?;
            for parent in locked_parents.iter_mut() {
                parent.detach(now);
            }
            Ok(core::mem::replace(current, replacement))
        })
    }

    fn validate_target(old: &View, root: &Location, target: &Location) -> Result<(), Error> {
        if target.mount().id() != root.mount().id() || target == root || old.covers(target) {
            return Err(Error::InvalidInput);
        }
        Ok(())
    }

    fn validate_destinations(
        root: &Location,
        replacements: &[Attachment],
        sponsor: &ResourceDomain,
    ) -> Result<(), Error> {
        let budget = ScratchBudget::new(sponsor);
        let filesystem = root.mount().filesystem();
        let mut paths = ScratchVec::new(budget.clone());
        paths.try_reserve_exact(replacements.len())?;
        for attachment in replacements
            .iter()
            .filter(|entry| entry.covered.mount().id() == root.mount().id())
        {
            paths.push((
                attachment.covered.node().get(),
                filesystem.ancestry(root.node(), attachment.covered.node(), &budget)?,
            ))?;
        }
        for (index, (node, _)) in paths.iter().enumerate() {
            for (other_index, (_, path)) in paths.iter().enumerate() {
                if index != other_index && path.iter().any(|(ancestor, _)| ancestor.get() == *node)
                {
                    return Err(Error::InvalidInput);
                }
            }
        }
        Ok(())
    }
}

fn is_mounted_root(view: &View, location: &Location) -> bool {
    location.node().get() == location.mount().root().get()
        && view
            .entries()
            .iter()
            .any(|entry| entry.mounted.id() == location.mount().id())
}

fn contains(root: &Location, location: &Location, budget: &ScratchBudget) -> Result<bool, Error> {
    if root.mount().filesystem().id() != location.mount().filesystem().id() {
        return Ok(false);
    }
    match root
        .mount()
        .filesystem()
        .ancestry(root.node(), location.node(), budget)
    {
        Ok(_) => Ok(true),
        Err(Error::Missing) => Ok(false),
        Err(error) => Err(error),
    }
}

/// A promoted subtree must not hide the new root, another source or a child
/// mount. Nested mounts need their own explicit topology operation first.
fn validate_promotion(
    old: &View,
    new_root: &Location,
    source: &Location,
    moves: &[(Location, Location)],
    budget: &ScratchBudget,
) -> Result<(), Error> {
    if !source.mount().filesystem().is_ramfs() {
        return Err(Error::Unsupported);
    }
    if source.node().get() == source.mount().root().get() || contains(source, new_root, budget)? {
        return Err(Error::InvalidInput);
    }
    for (other, _) in moves {
        if other != source && (contains(source, other, budget)? || contains(other, source, budget)?)
        {
            return Err(Error::InvalidInput);
        }
    }
    for attachment in old.entries() {
        if contains(source, &attachment.covered, budget)? {
            return Err(Error::Busy);
        }
    }
    Ok(())
}

/// At most `MAX_ROOT_MOVES` + 1 ordered sleeping locks are retained. RAM writers
/// publish odd epochs across both dirent removal and topology replacement;
/// remote target filesystems only need their existing stable-namespace lock.
fn with_namespace_locks<T>(
    locks: &[NamespaceLock],
    commit: impl FnOnce() -> Result<T, Error>,
) -> Result<T, Error> {
    let Some((first, rest)) = locks.split_first() else {
        return commit();
    };
    if first.edit {
        let _mutation = first.filesystem.ramfs()?.begin(Some(first.epoch))?;
        with_namespace_locks(rest, commit)
    } else {
        first
            .filesystem
            .with_namespace_stable(first.epoch, || with_namespace_locks(rest, commit))
    }
}
