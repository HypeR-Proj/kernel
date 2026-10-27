// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Immutable execution identity and kernel-prepared bootstrap mappings.

use crate::mm::user_space::UserAddress;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum MachineAbi {
    Aarch64,
    Riscv64,
    #[expect(dead_code, reason = "x86 Native image loading is not implemented")]
    X86_64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ImageError {
    InvalidEntry,
    InvalidStack,
}

/// Initial register values for one native user Thread.
///
/// A Process image supplies the first Thread's defaults. Additional Threads
/// must provide distinct stack/TLS values explicitly; silently reusing the
/// image stack would create overlapping execution state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct UserThreadStart {
    argument: u64,
    entry: UserAddress,
    stack: UserAddress,
    tls: UserAddress,
}

impl UserThreadStart {
    pub(crate) fn try_new(
        entry: UserAddress,
        stack: UserAddress,
        tls: UserAddress,
    ) -> Result<Self, ImageError> {
        if entry.get() == 0 {
            return Err(ImageError::InvalidEntry);
        }
        if stack.get() == 0 || stack.get() & 0xf != 0 {
            return Err(ImageError::InvalidStack);
        }
        Ok(Self {
            entry,
            stack,
            tls,
            argument: 0,
        })
    }

    pub(crate) const fn with_argument(mut self, argument: u64) -> Self {
        self.argument = argument;
        self
    }
    pub(crate) const fn argument(self) -> u64 {
        self.argument
    }

    pub(crate) const fn entry(self) -> UserAddress {
        self.entry
    }

    pub(crate) const fn stack(self) -> UserAddress {
        self.stack
    }

    pub(crate) const fn tls(self) -> UserAddress {
        self.tls
    }
}

/// Fixed execution identity for one installed image generation.
///
/// Native entry and stack values describe the userspace loader's initial
/// environment. Final application ELF metadata and auxv stay in userspace.
///
/// Mutable registers and mappings belong to `UserThread` and `NativeAddressSpace`,
/// respectively. Only Native process images can be constructed.
pub(crate) struct ProcessImage {
    machine: MachineAbi,
    entry: UserAddress,
    stack: UserAddress,
    tls: UserAddress,
    bootstrap: super::loader::BootstrapLayout,
    initial_stack_vmar: Option<crate::mm::user_space::Vmar>,
}

impl ProcessImage {
    pub(crate) fn try_native(
        machine: MachineAbi,
        entry: UserAddress,
        stack: UserAddress,
        tls: UserAddress,
    ) -> Result<Self, ImageError> {
        if entry.get() == 0 {
            return Err(ImageError::InvalidEntry);
        }
        if stack.get() == 0 || stack.get() & 0xf != 0 {
            return Err(ImageError::InvalidStack);
        }
        Ok(Self {
            machine,
            entry,
            stack,
            tls,
            bootstrap: super::loader::BootstrapLayout::default(),
            initial_stack_vmar: None,
        })
    }

    /// The address space owns the reservation; this token identifies the
    /// kernel-created temporary VMAR to wrap in a startup capability before first entry.
    pub(crate) fn with_bootstrap(
        mut self,
        vmar: crate::mm::user_space::Vmar,
        layout: super::loader::BootstrapLayout,
    ) -> Self {
        self.initial_stack_vmar = Some(vmar);
        self.bootstrap = layout;
        self
    }

    pub(crate) const fn initial_stack_vmar(&self) -> Option<crate::mm::user_space::Vmar> {
        self.initial_stack_vmar
    }

    pub(crate) const fn machine(&self) -> MachineAbi {
        self.machine
    }

    pub(crate) const fn bootstrap(&self) -> super::loader::BootstrapLayout {
        self.bootstrap
    }

    pub(crate) const fn initial_thread(&self) -> UserThreadStart {
        UserThreadStart {
            argument: 0,
            entry: self.entry,
            stack: self.stack,
            tls: self.tls,
        }
    }
}
