// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Minimal kernel-object module graph for host-side capability tests.

#[path = "../../../../src/kernel/authority.rs"]
pub(crate) mod authority;
#[path = "../../../../src/kernel/capability/mod.rs"]
pub(crate) mod capability;
pub(crate) use authority::Rights;

pub(crate) mod accounting {
    /// Capability-only host tests do not construct accounted transit batches.
    pub(crate) struct CommittedCharge;

    impl Drop for CommittedCharge {
        fn drop(&mut self) {}
    }
}

pub(crate) mod signals {
    use core::marker::PhantomData;

    /// Host tests in this harness exercise handle mechanics, not signals.
    #[derive(Clone, Copy)]
    pub(crate) struct SignalSource<'object>(PhantomData<&'object ()>);

    impl SignalSource<'_> {
        pub(crate) const fn for_test() -> Self {
            Self(PhantomData)
        }

        pub(super) const fn has_empty_mask(self) -> bool {
            false
        }
    }
}

#[path = "../../../../src/kernel/object/core.rs"]
mod object_core;

pub(crate) mod object {
    pub(crate) use super::object_core::{
        ActiveHandleError, ActiveHandleOwner, ErasedKernelRef, ExportPolicy, KernelObject,
        KernelRef, KernelService, Koid, ObjectHandleState, ObjectKind, ObjectPublication,
        ObjectReferenceSnapshot, ObjectRetirement, ObjectSnapshot, OperationPin, PublishableRef,
        Scheduler, UserExportableObject, final_reap_pending, private, reap_final_objects,
        reap_one_final_object,
    };
    pub(crate) use super::signals;
    pub(crate) use super::signals::SignalSource;
}
