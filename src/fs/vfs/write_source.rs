// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Fault-resolved inputs. Remote writes copy straight into a private source window.

use {
    crate::mm::user_space::DomainAccount, crate::mm::user_space::KernelPageBackend,
    crate::mm::user_space::NativeUserRead, crate::mm::user_space::WritableMappingLease,
    crate::mm::user_space::WritableVmo,
};

pub(super) enum WriteSource<'a> {
    #[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
    Bytes(&'a [u8]),
    User(&'a NativeUserRead, &'a crate::accounting::ResourceDomain),
}
impl WriteSource<'_> {
    pub(super) fn len(&self) -> usize {
        match self {
            #[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
            Self::Bytes(bytes) => bytes.len(),
            Self::User(input, _) => input.len() as usize,
        }
    }
    pub(super) fn copy_to_vmo(
        &self,
        memory: &WritableVmo<KernelPageBackend, DomainAccount>,
        offset: usize,
        length: usize,
    ) -> Result<(), crate::block::QueueError> {
        use crate::block::QueueError;
        match self {
            #[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
            Self::Bytes(bytes) => memory
                .write_exposed(
                    0,
                    bytes
                        .get(offset..offset.checked_add(length).ok_or(QueueError::Invalid)?)
                        .ok_or(QueueError::Invalid)?,
                )
                .map_err(|_| QueueError::Invalid),
            Self::User(input, _) => input
                .copy_to_vmo(memory, offset as u64, 0, length)
                .map_err(|_| QueueError::Invalid),
        }
    }
    pub(super) fn copy_to(&self, output: &mut [u8]) -> Result<(), super::instance::Error> {
        match self {
            #[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
            Self::Bytes(bytes) => {
                if bytes.len() != output.len() {
                    return Err(super::instance::Error::InvalidInput);
                }
                output.copy_from_slice(bytes);
                Ok(())
            }
            Self::User(input, _) => input
                .copy_to_slice(0, output)
                .map_err(|_| super::instance::Error::InvalidInput),
        }
    }

    pub(super) fn copy_to_exposed_vmo(
        &self,
        lease: &WritableMappingLease<KernelPageBackend, DomainAccount>,
        offset: usize,
        length: usize,
    ) -> Result<(), crate::block::QueueError> {
        use crate::block::QueueError;
        match self {
            #[cfg(all(feature = "kernel-test", feature = "kernel-self-test"))]
            Self::Bytes(bytes) => lease
                .write_exposed(
                    0,
                    bytes
                        .get(offset..offset.checked_add(length).ok_or(QueueError::Invalid)?)
                        .ok_or(QueueError::Invalid)?,
                )
                .map_err(|_| QueueError::Invalid),
            Self::User(input, _) => input
                .copy_to_exposed_vmo(lease, offset as u64, 0, length)
                .map_err(|_| QueueError::Invalid),
        }
    }
}
