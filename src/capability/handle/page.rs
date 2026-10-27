// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Exclusive, directly reclaimable page backing for handle storage.

use core::marker::PhantomData;
use core::ptr::NonNull;
use core::slice;

use super::HandleError;

pub(super) const PAGE_BYTES: usize = 4096;

/// Owns one page and its initialized prefix. No reference may outlive its owner.
pub(super) struct Page<T> {
    pointer: NonNull<T>,
    len: usize,
    _backing: Backing,
    _value: PhantomData<T>,
}

impl<T> Page<T> {
    pub(super) fn try_new(
        len: usize,
        mut initialize: impl FnMut(usize) -> T,
    ) -> Result<Self, HandleError> {
        if core::mem::size_of::<T>()
            .checked_mul(len)
            .is_none_or(|bytes| bytes > PAGE_BYTES)
            || core::mem::align_of::<T>() > PAGE_BYTES
        {
            return Err(HandleError::Allocation);
        }
        let backing = Backing::allocate()?;
        let pointer = backing.pointer().cast::<T>();
        let mut page = Self {
            pointer,
            len: 0,
            _backing: backing,
            _value: PhantomData,
        };
        for index in 0..len {
            let value = initialize(index);
            // SAFETY: backing owns one aligned writable page, len was checked
            // above, and each element is initialized once before publication.
            unsafe { page.pointer.as_ptr().add(index).write(value) };
            page.len += 1;
        }
        Ok(page)
    }

    pub(super) fn entries(&self) -> &[T] {
        // SAFETY: this owner keeps the initialized prefix alive and immutable
        // for this borrow; its backing is neither shared nor independently freed.
        unsafe { slice::from_raw_parts(self.pointer.as_ptr(), self.len) }
    }

    pub(super) fn entries_mut(&mut self) -> &mut [T] {
        // SAFETY: the exclusive owner borrow excludes all other access to this
        // initialized prefix, including destruction and page reclamation.
        unsafe { slice::from_raw_parts_mut(self.pointer.as_ptr(), self.len) }
    }
}

// SAFETY: the page uniquely owns its elements and backing, with no CPU affinity.
unsafe impl<T: Send> Send for Page<T> {}
// SAFETY: shared access exposes only shared borrows of initialized T values.
unsafe impl<T: Sync> Sync for Page<T> {}

impl<T> Drop for Page<T> {
    fn drop(&mut self) {
        // SAFETY: this is the unique destructor for the initialized prefix.
        // Backing is released only after the elements have been destroyed.
        unsafe {
            core::ptr::drop_in_place(core::ptr::slice_from_raw_parts_mut(
                self.pointer.as_ptr(),
                self.len,
            ))
        };
    }
}

#[cfg(not(test))]
struct Backing {
    _page: crate::mm::page_block::PageBlock,
    pointer: NonNull<u8>,
}

#[cfg(not(test))]
impl Backing {
    fn allocate() -> Result<Self, HandleError> {
        use crate::mm::page_block::PageBlock;
        let page = PageBlock::allocate(0).map_err(|_| HandleError::Allocation)?;
        let address = crate::mm::memory::linear_address(page.physical().get())
            .ok_or(HandleError::Allocation)?;
        let pointer = NonNull::new(address as *mut u8).ok_or(HandleError::Allocation)?;
        Ok(Self {
            _page: page,
            pointer,
        })
    }
    fn pointer(&self) -> NonNull<u8> {
        self.pointer
    }
}

// Host tests exercise the same typed storage and lifetime, using an exactly
// page-aligned system allocation in place of the kernel's physical page owner.
#[cfg(test)]
struct Backing(NonNull<u8>);

#[cfg(test)]
impl Backing {
    fn layout() -> core::alloc::Layout {
        match core::alloc::Layout::from_size_align(PAGE_BYTES, PAGE_BYTES) {
            Ok(layout) => layout,
            Err(_) => super::super::invariant_violation(),
        }
    }
    fn allocate() -> Result<Self, HandleError> {
        // SAFETY: layout is a nonzero page-sized, page-aligned allocation.
        NonNull::new(unsafe { alloc::alloc::alloc(Self::layout()) })
            .map(Self)
            .ok_or(HandleError::Allocation)
    }
    fn pointer(&self) -> NonNull<u8> {
        self.0
    }
}

#[cfg(test)]
impl Drop for Backing {
    fn drop(&mut self) {
        // SAFETY: this owner returns exactly the pointer/layout it allocated,
        // after Page has destroyed all initialized elements.
        unsafe { alloc::alloc::dealloc(self.0.as_ptr(), Self::layout()) };
    }
}
