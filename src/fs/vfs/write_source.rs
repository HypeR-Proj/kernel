// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Fault-resolved inputs. Remote writes copy straight into a private source window.

use {
    crate::mm::user_space::DomainAccount, crate::mm::user_space::KernelPageBackend,
    crate::mm::user_space::NativeUserRead, crate::mm::user_space::WritableMappingLease,
    crate::mm::user_space::WritableVmo,
};

mod fragments;
use fragments::FragmentSource;
pub(super) use fragments::WriteFragments;

pub(super) enum WriteSource<'a> {
    Bytes(&'a [u8]),
    Fragments(FragmentSource<'a>),
    User(&'a NativeUserRead, &'a crate::accounting::ResourceDomain),
    UserRange(
        &'a NativeUserRead,
        &'a crate::accounting::ResourceDomain,
        usize,
        usize,
    ),
}
impl WriteSource<'_> {
    pub(super) fn fragments(
        owner: &dyn WriteFragments,
    ) -> Result<WriteSource<'_>, super::instance::Error> {
        FragmentSource::new(owner)
            .map(WriteSource::Fragments)
            .ok_or(super::instance::Error::InvalidInput)
    }
    pub(super) fn len(&self) -> usize {
        match self {
            Self::Bytes(bytes) => bytes.len(),
            Self::Fragments(source) => source.len(),
            Self::User(input, _) => input.len() as usize,
            Self::UserRange(_, _, _, length) => *length,
        }
    }
    /// Borrow a checked subrange without copying or resolving user pages again.
    pub(super) fn slice(
        &self,
        offset: usize,
        length: usize,
    ) -> Result<WriteSource<'_>, super::instance::Error> {
        let end = offset
            .checked_add(length)
            .filter(|end| *end <= self.len())
            .ok_or(super::instance::Error::InvalidInput)?;
        Ok(match self {
            Self::Bytes(bytes) => WriteSource::Bytes(&bytes[offset..end]),
            Self::Fragments(source) => WriteSource::Fragments(
                source
                    .slice(offset, length)
                    .ok_or(super::instance::Error::InvalidInput)?,
            ),
            Self::User(input, domain) => WriteSource::UserRange(input, domain, offset, length),
            Self::UserRange(input, domain, first, _) => WriteSource::UserRange(
                input,
                domain,
                first
                    .checked_add(offset)
                    .ok_or(super::instance::Error::InvalidInput)?,
                length,
            ),
        })
    }
    fn range_offset(&self, offset: usize, length: usize) -> Option<u64> {
        offset
            .checked_add(length)
            .filter(|end| *end <= self.len())?;
        let first = match self {
            Self::UserRange(_, _, first, _) => *first,
            _ => 0,
        };
        u64::try_from(first.checked_add(offset)?).ok()
    }
    pub(super) fn copy_to_vmo(
        &self,
        memory: &WritableVmo<KernelPageBackend, DomainAccount>,
        offset: usize,
        length: usize,
    ) -> Result<(), crate::block::QueueError> {
        use crate::block::QueueError;
        match self {
            Self::Fragments(source) => source
                .copy(offset, length, |at, bytes| {
                    memory.write_exposed(at as u64, bytes)
                })
                .map_err(|_| QueueError::Invalid),
            Self::Bytes(bytes) => memory
                .write_exposed(
                    0,
                    bytes
                        .get(offset..offset.checked_add(length).ok_or(QueueError::Invalid)?)
                        .ok_or(QueueError::Invalid)?,
                )
                .map_err(|_| QueueError::Invalid),
            Self::User(input, _) | Self::UserRange(input, _, _, _) => input
                .copy_to_vmo(
                    memory,
                    self.range_offset(offset, length)
                        .ok_or(QueueError::Invalid)?,
                    0,
                    length,
                )
                .map_err(|_| QueueError::Invalid),
        }
    }
    pub(super) fn copy_range(
        &self,
        offset: usize,
        output: &mut [u8],
    ) -> Result<(), super::instance::Error> {
        match self {
            Self::Fragments(source) => source
                .copy(offset, output.len(), |at, bytes| {
                    output[at..at + bytes.len()].copy_from_slice(bytes);
                    Ok::<_, ()>(())
                })
                .map_err(|_| super::instance::Error::InvalidInput),
            Self::Bytes(bytes) => {
                let end = offset
                    .checked_add(output.len())
                    .ok_or(super::instance::Error::InvalidInput)?;
                output.copy_from_slice(
                    bytes
                        .get(offset..end)
                        .ok_or(super::instance::Error::InvalidInput)?,
                );
                Ok(())
            }
            Self::User(input, _) | Self::UserRange(input, _, _, _) => input
                .copy_to_slice(
                    self.range_offset(offset, output.len())
                        .ok_or(super::instance::Error::InvalidInput)?,
                    output,
                )
                .map_err(|_| super::instance::Error::InvalidInput),
        }
    }

    pub(super) fn copy_to(&self, output: &mut [u8]) -> Result<(), super::instance::Error> {
        if self.len() != output.len() {
            return Err(super::instance::Error::InvalidInput);
        }
        self.copy_range(0, output)
    }

    pub(super) fn copy_to_exposed_vmo(
        &self,
        lease: &WritableMappingLease<KernelPageBackend, DomainAccount>,
        offset: usize,
        length: usize,
    ) -> Result<(), crate::block::QueueError> {
        use crate::block::QueueError;
        match self {
            Self::Fragments(source) => source
                .copy(offset, length, |at, bytes| {
                    lease.write_exposed(at as u64, bytes)
                })
                .map_err(|_| QueueError::Invalid),
            Self::Bytes(bytes) => lease
                .write_exposed(
                    0,
                    bytes
                        .get(offset..offset.checked_add(length).ok_or(QueueError::Invalid)?)
                        .ok_or(QueueError::Invalid)?,
                )
                .map_err(|_| QueueError::Invalid),
            Self::User(input, _) | Self::UserRange(input, _, _, _) => input
                .copy_to_exposed_vmo(
                    lease,
                    self.range_offset(offset, length)
                        .ok_or(QueueError::Invalid)?,
                    0,
                    length,
                )
                .map_err(|_| QueueError::Invalid),
        }
    }
}
