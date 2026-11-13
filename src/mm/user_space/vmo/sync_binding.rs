// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Backing-owned synchronization attribution, shared by every writable alias.

use alloc::boxed::Box;

/// Domain lifetime and binding accounting retained until backing retirement.
/// The generic VMO layer never interprets or invokes this owner.
pub(crate) trait BindingOwner: Send + Sync {}
impl<T: Send + Sync> BindingOwner for T {}

pub(crate) struct SyncBinding {
    pub(crate) domain: u64,
    pub(crate) offset: u64,
    pub(crate) end: u64,
    _owner: Box<dyn BindingOwner>,
    next: Option<Box<Self>>,
}

impl SyncBinding {
    pub(crate) fn new(domain: u64, offset: u64, end: u64, owner: Box<dyn BindingOwner>) -> Self {
        Self {
            domain,
            offset,
            end,
            _owner: owner,
            next: None,
        }
    }
}

pub(crate) struct SyncBindings {
    head: Option<Box<SyncBinding>>,
}

impl SyncBindings {
    pub(crate) const fn new() -> Self {
        Self { head: None }
    }

    /// All storage is prepared before this lock-protected publication. Return
    /// rejected or redundant ownership to the caller for destruction unlocked.
    pub(crate) fn insert(
        &mut self,
        mut binding: Box<SyncBinding>,
    ) -> Result<Option<Box<SyncBinding>>, Box<SyncBinding>> {
        let mut cursor = self.head.as_deref();
        while let Some(existing) = cursor {
            if existing.offset < binding.end && binding.offset < existing.end {
                if existing.domain != binding.domain {
                    return Err(binding);
                }
                if existing.offset <= binding.offset && existing.end >= binding.end {
                    return Ok(Some(binding));
                }
            }
            cursor = existing.next.as_deref();
        }
        binding.next = self.head.take();
        self.head = Some(binding);
        Ok(None)
    }

    pub(crate) fn contains(&self, domain: u64, offset: u64, end: u64) -> bool {
        let mut cursor = self.head.as_deref();
        while let Some(binding) = cursor {
            if binding.domain == domain && binding.offset <= offset && binding.end >= end {
                return true;
            }
            cursor = binding.next.as_deref();
        }
        false
    }
}

impl Drop for SyncBindings {
    fn drop(&mut self) {
        // No recursion proportional to untrusted binding count.
        while let Some(mut binding) = self.head.take() {
            self.head = binding.next.take();
            drop(binding);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn binding(domain: u64, start: u64, end: u64) -> Box<SyncBinding> {
        Box::new(SyncBinding::new(domain, start, end, Box::new(())))
    }
    #[test]
    fn overlap_is_attributed_once_but_disjoint_domains_are_allowed() {
        let mut bindings = SyncBindings::new();
        assert!(bindings.insert(binding(1, 0, 64)).is_ok());
        assert!(bindings.insert(binding(1, 32, 96)).is_ok());
        assert!(bindings.insert(binding(2, 64, 128)).is_err());
        assert!(bindings.insert(binding(2, 96, 128)).is_ok());
        assert!(bindings.contains(1, 56, 64));
        assert!(!bindings.contains(2, 56, 64));
    }
}
