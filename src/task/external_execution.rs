// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Runtime type witness for scheduler-owned subsystem execution payloads.

use core::any::TypeId;
use core::cell::UnsafeCell;
use core::ptr::NonNull;

/// Copyable identity for a stable, type-erased execution allocation.
///
/// Copying this value does not extend the allocation lifetime. Scheduler
/// ownership and detachment rules keep the payload alive while an observation
/// of the current thread exists.
#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) struct ExternalExecutionPointer {
    pointer: NonNull<()>,
    type_id: TypeId,
}

impl ExternalExecutionPointer {
    pub(crate) fn from_cell<T: 'static>(pointer: NonNull<UnsafeCell<T>>) -> Self {
        Self {
            pointer: pointer.cast(),
            type_id: TypeId::of::<T>(),
        }
    }

    /// Recovers a typed raw pointer only when the construction witness agrees.
    pub(crate) fn downcast<T: 'static>(self) -> Option<NonNull<T>> {
        if self.type_id != TypeId::of::<T>() {
            return None;
        }
        let pointer = UnsafeCell::raw_get(self.pointer.cast::<UnsafeCell<T>>().as_ptr());
        // SAFETY: `from_cell` accepted a non-null pointer and `raw_get`
        // preserves the address through UnsafeCell's transparent wrapper.
        Some(unsafe { NonNull::new_unchecked(pointer) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_type_recovers_the_original_stable_address() {
        let mut value = UnsafeCell::new(37_u64);
        let source = NonNull::from(&mut value);
        let witness = ExternalExecutionPointer::from_cell(source);
        let recovered = witness.downcast::<u64>();
        assert_eq!(recovered.map(NonNull::as_ptr), Some(value.get()));
    }

    #[test]
    fn wrong_type_cannot_manufacture_a_typed_pointer() {
        let mut value = UnsafeCell::new(37_u64);
        let witness = ExternalExecutionPointer::from_cell(NonNull::from(&mut value));
        assert!(witness.downcast::<u32>().is_none());
    }
}
