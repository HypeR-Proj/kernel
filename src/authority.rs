// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Architecture-neutral authority vocabulary shared by object policy and handles.

use hyper::abi::native;

/// Object support ceiling and monotonically attenuated process-handle grant.
///
/// `Rights` is the ABI-shaped union used at syscall and object boundaries.
/// Handle storage decomposes it into operation and propagation authority so a
/// transfer check cannot accidentally treat `DUPLICATE` or `TRANSFER` as an
/// ordinary object operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Rights(u64);

/// Authority to invoke operations on the referenced object.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct OperationRights(u64);

/// Authority to derive or relocate a process-local handle.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct PropagationRights(u64);

/// Compiler-visible decomposition of one handle's complete authority.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct HandleRights {
    operations: OperationRights,
    propagation: PropagationRights,
}

impl Rights {
    #[cfg(test)]
    pub(crate) const fn is_empty(self) -> bool {
        self.0 == 0
    }

    pub(crate) const NONE: Self = Self(0);
    pub(crate) const DUPLICATE: Self = Self(native::HYPER_NATIVE_RIGHT_DUPLICATE);
    pub(crate) const TRANSFER: Self = Self(native::HYPER_NATIVE_RIGHT_TRANSFER);
    pub(crate) const WAIT: Self = Self(native::HYPER_NATIVE_RIGHT_WAIT);
    pub(crate) const INSPECT: Self = Self(native::HYPER_NATIVE_RIGHT_INSPECT);
    pub(crate) const INSPECT_DETAILS: Self = Self(native::HYPER_NATIVE_RIGHT_INSPECT_DETAILS);
    pub(crate) const READ: Self = Self(native::HYPER_NATIVE_RIGHT_READ);
    pub(crate) const WRITE: Self = Self(native::HYPER_NATIVE_RIGHT_WRITE);
    pub(crate) const MAP: Self = Self(native::HYPER_NATIVE_RIGHT_MAP);
    pub(crate) const EXECUTE: Self = Self(native::HYPER_NATIVE_RIGHT_EXECUTE);
    pub(crate) const START: Self = Self(native::HYPER_NATIVE_RIGHT_START);
    pub(crate) const REQUEST_STOP: Self = Self(native::HYPER_NATIVE_RIGHT_REQUEST_STOP);
    pub(crate) const MAP_DMA: Self = Self(native::HYPER_NATIVE_RIGHT_MAP_DMA);
    pub(crate) const ACK_INTERRUPT: Self = Self(native::HYPER_NATIVE_RIGHT_ACK_INTERRUPT);
    pub(crate) const ASSIGN_DEVICE: Self = Self(native::HYPER_NATIVE_RIGHT_ASSIGN_DEVICE);
    pub(crate) const REVOKE: Self = Self(native::HYPER_NATIVE_RIGHT_REVOKE);
    pub(crate) const SIGNAL: Self = Self(native::HYPER_NATIVE_RIGHT_SIGNAL);
    pub(crate) const CREATE_PROCESS: Self = Self(native::HYPER_NATIVE_RIGHT_CREATE_PROCESS);
    pub(crate) const CREATE_THREAD: Self = Self(native::HYPER_NATIVE_RIGHT_CREATE_THREAD);
    pub(crate) const CREATE_OBJECT: Self = Self(native::HYPER_NATIVE_RIGHT_CREATE_OBJECT);
    pub(crate) const CREATE_TASK_GROUP: Self = Self(native::HYPER_NATIVE_RIGHT_CREATE_TASK_GROUP);
    pub(crate) const CREATE_RESOURCE_DOMAIN: Self =
        Self(native::HYPER_NATIVE_RIGHT_CREATE_RESOURCE_DOMAIN);
    pub(crate) const SET_LIMITS: Self = Self(native::HYPER_NATIVE_RIGHT_SET_LIMITS);
    #[allow(
        dead_code,
        reason = "reserved executable authority is only constructed by self-tests"
    )]
    pub(crate) const CREATE_EXECUTABLE: Self = Self(native::HYPER_NATIVE_RIGHT_CREATE_EXECUTABLE);
    /// Permits attaching a newly constructed Process to this `TaskGroup`.
    pub(crate) const TASK_GROUP_ATTACH_PROCESS: Self =
        Self(native::HYPER_NATIVE_RIGHT_TASK_GROUP_ATTACH_PROCESS);
    /// Permits charging a newly constructed Process to this `ResourceDomain`.
    pub(crate) const RESOURCE_DOMAIN_SPONSOR: Self =
        Self(native::HYPER_NATIVE_RIGHT_RESOURCE_DOMAIN_SPONSOR);
    pub(crate) const DERIVE: Self = Self(native::HYPER_NATIVE_RIGHT_DERIVE);
    pub(crate) const CREATE_VIRTUAL_MACHINE: Self =
        Self(native::HYPER_NATIVE_RIGHT_CREATE_VIRTUAL_MACHINE);
    pub(crate) const BIND_WAIT: Self = Self(native::HYPER_NATIVE_RIGHT_BIND_WAIT);
    pub(crate) const SET_ATTRIBUTES: Self = Self(native::HYPER_NATIVE_RIGHT_SET_ATTRIBUTES);
    pub(crate) const LOCK_FILE: Self = Self(native::HYPER_NATIVE_RIGHT_LOCK_FILE);
    pub(crate) const MANAGE_NAMESPACE: Self = Self(native::HYPER_NATIVE_RIGHT_MANAGE_NAMESPACE);
    pub(crate) const PROTECT: Self = Self(native::HYPER_NATIVE_RIGHT_PROTECT);
    pub(crate) const LOCK_MEMORY: Self = Self(native::HYPER_NATIVE_RIGHT_LOCK_MEMORY);
    pub(crate) const RETAIN_PROCESS: Self = Self(native::HYPER_NATIVE_RIGHT_RETAIN_PROCESS);
    pub(crate) const RETIRE: Self = Self(native::HYPER_NATIVE_RIGHT_RETIRE);
    pub(crate) const SCHED_LOWER: Self = Self(native::HYPER_NATIVE_RIGHT_SCHED_LOWER);
    pub(crate) const SCHED_PRIORITY: Self = Self(native::HYPER_NATIVE_RIGHT_SCHED_PRIORITY);
    pub(crate) const SCHED_CLASS: Self = Self(native::HYPER_NATIVE_RIGHT_SCHED_CLASS);
    pub(crate) const SCHED_BUDGET: Self = Self(native::HYPER_NATIVE_RIGHT_SCHED_BUDGET);

    pub(crate) const KNOWN: Self = Self(native::HYPER_NATIVE_RIGHTS_MASK);
    const PROPAGATION_MASK: u64 = Self::DUPLICATE.0 | Self::TRANSFER.0;

    pub(crate) const fn from_bits(bits: u64) -> Option<Self> {
        if bits & !Self::KNOWN.0 == 0 {
            Some(Self(bits))
        } else {
            None
        }
    }

    pub(crate) const fn bits(self) -> u64 {
        self.0
    }

    pub(crate) const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    pub(crate) const fn intersection(self, other: Self) -> Self {
        Self(self.0 & other.0)
    }

    pub(crate) const fn contains(self, required: Self) -> bool {
        self.intersection(required).0 == required.0
    }

    pub(crate) const fn decompose(self) -> HandleRights {
        HandleRights {
            operations: OperationRights(self.0 & !Self::PROPAGATION_MASK),
            propagation: PropagationRights(self.0 & Self::PROPAGATION_MASK),
        }
    }
}

impl HandleRights {
    pub(crate) const fn union(self) -> Rights {
        Rights(self.operations.0 | self.propagation.0)
    }

    pub(crate) const fn contains(self, required: Self) -> bool {
        self.operations.contains(required.operations)
            && self.propagation.contains(required.propagation)
    }

    pub(crate) const fn propagation(self) -> PropagationRights {
        self.propagation
    }
}

impl OperationRights {
    pub(crate) const fn contains(self, required: Self) -> bool {
        self.0 & required.0 == required.0
    }
}

impl PropagationRights {
    pub(crate) const DUPLICATE: Self = Self(Rights::DUPLICATE.0);
    pub(crate) const TRANSFER: Self = Self(Rights::TRANSFER.0);

    pub(crate) const fn contains(self, required: Self) -> bool {
        self.0 & required.0 == required.0
    }
}

impl Default for Rights {
    fn default() -> Self {
        Self::NONE
    }
}
