// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Owned, bounded pending state for iterative path resolution.

use crate::fs::primitives::file_data::StorageBudget;
use {
    crate::fs::primitives::scratch::BudgetedVec,
    crate::fs::primitives::scratch::Error as BufferError,
};

use {
    crate::fs::primitives::MAX_PATH_BYTES, crate::fs::primitives::MAX_PATH_COMPONENTS,
    crate::fs::primitives::Name, crate::fs::primitives::Path, crate::fs::primitives::PathComponent,
};

const MAX_SYMLINK_HOPS: usize = 40;
const MAX_EXPANDED_PATH_BYTES: usize = MAX_PATH_BYTES * 4;
pub(super) const MAX_RETAINED_NAME_BYTES: usize = MAX_EXPANDED_PATH_BYTES + MAX_PATH_BYTES;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum StateError<E = ()> {
    Allocation,
    InvalidPath,
    SymlinkLoop,
    Budget(E),
}

impl<E> From<BufferError<E>> for StateError<E> {
    fn from(error: BufferError<E>) -> Self {
        match error {
            BufferError::Allocation => Self::Allocation,
            BufferError::Size => Self::InvalidPath,
            BufferError::Budget(error) => Self::Budget(error),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum PendingComponent {
    Current,
    Parent,
    Name(NameDescriptor),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct NameDescriptor {
    start: u16,
    length: u8,
}

pub(super) struct PendingPath<B: StorageBudget> {
    components: BudgetedVec<PendingComponent, B>,
    names: BudgetedVec<u8, B>,
    expanded_components: usize,
    expanded_bytes: usize,
    symlink_hops: usize,
}

/// Kernel-owned ancestry for one capability-confined traversal.
///
/// Backends supply child identities but never supply trusted parent movement.
/// Keeping the visited chain here makes `..` and absolute symlink handling
/// independent of backend integrity.
pub(super) struct Traversal<T, B: StorageBudget> {
    root: T,
    current: T,
    ancestors: BudgetedVec<T, B>,
}

impl<T: Clone + Eq, B: StorageBudget> Traversal<T, B> {
    pub(super) fn try_new(root: T, budget: B) -> Result<Self, StateError<B::Error>> {
        let mut ancestors = BudgetedVec::new(budget);
        ancestors.try_reserve_exact(1)?;
        Ok(Self {
            current: root.clone(),
            root,
            ancestors,
        })
    }

    pub(super) const fn current(&self) -> &T {
        &self.current
    }

    pub(super) fn visited(&self) -> impl Iterator<Item = &T> {
        self.ancestors.iter().chain(core::iter::once(&self.current))
    }

    pub(super) fn descend(&mut self, child: T) -> Result<(), StateError<B::Error>> {
        if self.ancestors.len() == MAX_PATH_COMPONENTS {
            return Err(StateError::InvalidPath);
        }
        self.ancestors.try_reserve(1)?;
        let parent = core::mem::replace(&mut self.current, child);
        self.ancestors.push(parent)?;
        Ok(())
    }

    pub(super) fn parent(&mut self) {
        if self.current == self.root {
            self.ancestors.clear();
            return;
        }
        self.current = match self.ancestors.pop() {
            Some(parent) => parent,
            None => self.root.clone(),
        };
    }

    pub(super) fn restart(&mut self) {
        self.current = self.root.clone();
        self.ancestors.clear();
    }
}

impl<B: StorageBudget + Clone> PendingPath<B> {
    pub(super) fn new(path: Path<'_>, budget: B) -> Result<Self, StateError<B::Error>> {
        let mut state = Self {
            components: BudgetedVec::new(budget.clone()),
            names: BudgetedVec::new(budget),
            expanded_components: path.component_count(),
            expanded_bytes: path.as_str().len(),
            symlink_hops: 0,
        };
        state.push(path)?;
        Ok(state)
    }

    pub(super) fn retain_name(
        &mut self,
        name: Name<'_>,
    ) -> Result<NameDescriptor, StateError<B::Error>> {
        let start = u16::try_from(self.names.len()).map_err(|_| StateError::InvalidPath)?;
        let length = u8::try_from(name.as_bytes().len()).map_err(|_| StateError::InvalidPath)?;
        if self
            .names
            .len()
            .checked_add(name.as_bytes().len())
            .is_none_or(|length| length > MAX_RETAINED_NAME_BYTES)
        {
            return Err(StateError::InvalidPath);
        }
        self.names.try_reserve(name.as_bytes().len())?;
        self.names.extend_from_slice(name.as_bytes())?;
        Ok(NameDescriptor { start, length })
    }

    /// Require a directory after the initial pathname without inventing a
    /// pathname component or consuming the caller's byte/depth budget.
    pub(super) fn require_final_directory(&mut self) -> Result<(), StateError<B::Error>> {
        self.components.push(PendingComponent::Current)?;
        self.components.rotate_right(1);
        Ok(())
    }

    pub(super) fn is_empty(&self) -> bool {
        self.components.is_empty()
    }

    pub(super) fn pop(&mut self) -> Option<PendingComponent> {
        self.components.pop()
    }

    pub(super) fn name(&self, descriptor: NameDescriptor) -> Option<Name<'_>> {
        let start = usize::from(descriptor.start);
        let end = start.checked_add(usize::from(descriptor.length))?;
        let bytes = self.names.get(start..end)?;
        let value = core::str::from_utf8(bytes).ok()?;
        Name::new(value).ok()
    }

    /// Inserts a symlink target ahead of the unresolved suffix.
    ///
    /// The returned flag tells namespace policy whether traversal must restart
    /// at its capability root. Components are copied before the caller's
    /// backend buffer is released, so no adapter needs to lend stable storage.
    pub(super) fn expand_symlink(
        &mut self,
        target: &str,
        directory_required: bool,
    ) -> Result<bool, StateError<B::Error>> {
        self.symlink_hops = self
            .symlink_hops
            .checked_add(1)
            .ok_or(StateError::InvalidPath)?;
        if self.symlink_hops > MAX_SYMLINK_HOPS {
            return Err(StateError::SymlinkLoop);
        }
        let target = Path::new(target).map_err(|_| StateError::InvalidPath)?;
        self.expanded_bytes = self
            .expanded_bytes
            .checked_add(target.as_str().len())
            .ok_or(StateError::InvalidPath)?;
        if self.expanded_bytes > MAX_EXPANDED_PATH_BYTES {
            return Err(StateError::InvalidPath);
        }
        self.expanded_components = self
            .expanded_components
            .checked_add(target.component_count())
            .ok_or(StateError::InvalidPath)?;
        if self.expanded_components > MAX_PATH_COMPONENTS {
            return Err(StateError::InvalidPath);
        }
        let absolute = target.is_absolute();
        if directory_required {
            self.components.push(PendingComponent::Current)?;
        }
        self.push(target)?;
        Ok(absolute)
    }

    fn push(&mut self, path: Path<'_>) -> Result<(), StateError<B::Error>> {
        let name_bytes = path.components().try_fold(0usize, |total, component| {
            let length = match component {
                PathComponent::Name(name) => name.as_bytes().len(),
                PathComponent::Current | PathComponent::Parent => 0,
            };
            total.checked_add(length).ok_or(StateError::InvalidPath)
        })?;
        if self
            .names
            .len()
            .checked_add(name_bytes)
            .is_none_or(|length| length > MAX_RETAINED_NAME_BYTES)
        {
            return Err(StateError::InvalidPath);
        }
        self.components.try_reserve(path.component_count())?;
        self.names.try_reserve(name_bytes)?;
        let start = self.components.len();
        for component in path.components() {
            let owned = match component {
                PathComponent::Current => PendingComponent::Current,
                PathComponent::Parent => PendingComponent::Parent,
                PathComponent::Name(name) => PendingComponent::Name(self.retain_name(name)?),
            };
            self.components.push(owned)?;
        }
        let added = self
            .components
            .get_mut(start..)
            .ok_or(StateError::Allocation)?;
        added.reverse();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use alloc::format;

    use super::{
        MAX_EXPANDED_PATH_BYTES, MAX_SYMLINK_HOPS, PendingComponent, PendingPath, StateError,
        Traversal,
    };
    use {
        crate::fs::primitives::MAX_PATH_BYTES, crate::fs::primitives::MAX_PATH_COMPONENTS,
        crate::fs::primitives::Path,
    };

    #[derive(Clone, Copy)]
    struct Budget;
    impl crate::fs::primitives::file_data::StorageBudget for Budget {
        type Charge = ();
        type Error = ();
        fn reserve(&self, _: usize) -> Result<(), ()> {
            Ok(())
        }
    }

    fn path(value: &str) -> Path<'_> {
        match Path::new(value) {
            Ok(path) => path,
            Err(error) => panic!("test path is invalid: {error:?}"),
        }
    }

    #[test]
    fn symlink_target_precedes_the_existing_suffix() {
        let mut pending = match PendingPath::new(path("tail"), Budget) {
            Ok(pending) => pending,
            Err(error) => panic!("pending path failed: {error:?}"),
        };
        assert_eq!(pending.expand_symlink("directory/file", false), Ok(false));
        let first = match pending.pop() {
            Some(PendingComponent::Name(name)) => name,
            other => panic!("unexpected first component: {other:?}"),
        };
        assert_eq!(
            pending.name(first).map(|name| name.as_str()),
            Some("directory")
        );
        let second = match pending.pop() {
            Some(PendingComponent::Name(name)) => name,
            other => panic!("unexpected second component: {other:?}"),
        };
        assert_eq!(pending.name(second).map(|name| name.as_str()), Some("file"));
        let third = match pending.pop() {
            Some(PendingComponent::Name(name)) => name,
            other => panic!("unexpected third component: {other:?}"),
        };
        assert_eq!(pending.name(third).map(|name| name.as_str()), Some("tail"));
        assert!(pending.is_empty());
    }

    #[test]
    fn absolute_targets_are_reported_without_ambient_root_state() {
        let mut pending = match PendingPath::new(path("tail"), Budget) {
            Ok(pending) => pending,
            Err(error) => panic!("pending path failed: {error:?}"),
        };
        assert_eq!(pending.expand_symlink("/etc/config", false), Ok(true));
    }

    #[test]
    fn expansion_work_and_symlink_hops_are_bounded() {
        let mut pending = match PendingPath::new(path("tail"), Budget) {
            Ok(pending) => pending,
            Err(error) => panic!("pending path failed: {error:?}"),
        };
        let maximum_target = (0..256)
            .map(|index| format!("n{index}"))
            .collect::<alloc::vec::Vec<_>>()
            .join("/");
        assert_eq!(
            pending.expand_symlink(&maximum_target, false),
            Err(StateError::InvalidPath)
        );

        let mut pending = match PendingPath::new(path("tail"), Budget) {
            Ok(pending) => pending,
            Err(error) => panic!("pending path failed: {error:?}"),
        };
        for _ in 0..MAX_SYMLINK_HOPS {
            assert_eq!(pending.expand_symlink(".", false), Ok(false));
        }
        assert_eq!(
            pending.expand_symlink(".", false),
            Err(StateError::SymlinkLoop)
        );

        let target = alloc::vec!["x".repeat(255); 16].join("/");
        let mut pending = match PendingPath::new(path("tail"), Budget) {
            Ok(pending) => pending,
            Err(error) => panic!("pending path failed: {error:?}"),
        };
        for _ in 0..4 {
            assert_eq!(pending.expand_symlink(&target, false), Ok(false));
        }
        assert_eq!(
            pending.expand_symlink(&target, false),
            Err(StateError::InvalidPath)
        );
        assert_eq!(MAX_EXPANDED_PATH_BYTES, MAX_PATH_BYTES * 4);
    }

    #[test]
    fn traversal_owns_parent_clamping_and_absolute_restart() {
        let mut traversal = match Traversal::try_new(10_u64, Budget) {
            Ok(traversal) => traversal,
            Err(error) => panic!("traversal construction failed: {error:?}"),
        };
        traversal.parent();
        assert_eq!(*traversal.current(), 10);
        assert_eq!(traversal.descend(11), Ok(()));
        assert_eq!(traversal.descend(12), Ok(()));
        assert_eq!(
            traversal.visited().copied().collect::<alloc::vec::Vec<_>>(),
            alloc::vec![10, 11, 12]
        );
        traversal.parent();
        assert_eq!(*traversal.current(), 11);
        traversal.restart();
        assert_eq!(*traversal.current(), 10);
        traversal.parent();
        assert_eq!(*traversal.current(), 10);
    }
    #[test]
    fn rooted_start_and_suffix_share_one_depth_budget() {
        let mut traversal = match Traversal::try_new(0_usize, Budget) {
            Ok(value) => value,
            Err(error) => panic!("traversal construction failed: {error:?}"),
        };
        for depth in 1..=MAX_PATH_COMPONENTS {
            assert_eq!(traversal.descend(depth), Ok(()));
        }
        assert_eq!(
            traversal.descend(MAX_PATH_COMPONENTS + 1),
            Err(StateError::InvalidPath)
        );
        assert_eq!(*traversal.current(), MAX_PATH_COMPONENTS);
        traversal.parent();
        assert_eq!(traversal.descend(MAX_PATH_COMPONENTS + 1), Ok(()));
    }
    #[test]
    fn terminal_directory_requirements_preserve_component_order() {
        let mut pending = match PendingPath::new(path("leaf"), Budget) {
            Ok(value) => value,
            Err(error) => panic!("pending path: {error:?}"),
        };
        assert_eq!(pending.require_final_directory(), Ok(()));
        assert!(matches!(pending.pop(), Some(PendingComponent::Name(_))));
        assert_eq!(pending.pop(), Some(PendingComponent::Current));
        assert!(pending.is_empty());
        let mut pending = match PendingPath::new(path("suffix"), Budget) {
            Ok(value) => value,
            Err(error) => panic!("pending path: {error:?}"),
        };
        assert_eq!(pending.expand_symlink("target", true), Ok(false));
        assert!(matches!(pending.pop(), Some(PendingComponent::Name(_))));
        assert_eq!(pending.pop(), Some(PendingComponent::Current));
        let descriptor = match pending.pop() {
            Some(PendingComponent::Name(value)) => value,
            other => panic!("suffix: {other:?}"),
        };
        assert_eq!(
            pending.name(descriptor).map(|name| name.as_str()),
            Some("suffix")
        );
    }

    #[test]
    fn terminal_separator_does_not_consume_path_component_or_byte_limits() {
        let components = alloc::vec!["x";MAX_PATH_COMPONENTS].join("/");
        let maximum_bytes = alloc::vec!["x".repeat(255);16].join("/");
        assert_eq!(maximum_bytes.len() + 1, MAX_PATH_BYTES);
        for value in [&components, &maximum_bytes] {
            let parsed = path(value);
            let count = parsed.component_count();
            let mut pending = match PendingPath::new(parsed, Budget) {
                Ok(value) => value,
                Err(error) => panic!("pending path: {error:?}"),
            };
            assert_eq!(pending.require_final_directory(), Ok(()));
            for _ in 0..count {
                assert!(matches!(pending.pop(), Some(PendingComponent::Name(_))));
            }
            assert_eq!(pending.pop(), Some(PendingComponent::Current));
            assert!(pending.is_empty());
        }
    }
}
