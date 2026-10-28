// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! CPU-owned request storage. Only the dedicated exchange pool is visible to DMA.

use crate::block::{Device, Error, window::WindowLease};
use alloc::vec::Vec;
use core::ptr::NonNull;

pub(in crate::block) enum Payload {
    Owned(Vec<u8>),
    Window {
        lease: WindowLease,
        ranges: Vec<(usize, usize)>,
    },
    Scatter(Vec<Destination>),
}
impl Default for Payload {
    fn default() -> Self {
        Self::Owned(Vec::new())
    }
}

/// A kernel-only synchronous borrow. Construction is restricted to the joined
/// scatter-read path; completion must be joined even when waiting fails.
pub(in crate::block) struct Destination {
    pub(in crate::block) address: NonNull<u8>,
    pub(in crate::block) length: usize,
}
// SAFETY: Only the device pump accesses these exclusively borrowed bytes, and
// the submitting thread joins that access before its mutable borrow can end.
unsafe impl Send for Destination {}

impl Payload {
    pub(in crate::block) fn allocation_bytes(&self) -> Option<usize> {
        match self {
            Self::Owned(bytes) => Some(bytes.capacity()),
            Self::Window { ranges, .. } => ranges
                .capacity()
                .checked_mul(core::mem::size_of::<(usize, usize)>()),
            Self::Scatter(parts) => parts
                .capacity()
                .checked_mul(core::mem::size_of::<Destination>()),
        }
    }

    pub(in crate::block) fn owned_mut(&mut self) -> &mut [u8] {
        match self {
            Self::Owned(bytes) => bytes,
            _ => &mut [],
        }
    }
    pub(in crate::block) fn owned(&self) -> &[u8] {
        match self {
            Self::Owned(bytes) => bytes,
            _ => &[],
        }
    }
    pub(in crate::block) fn finish(&mut self) {
        // Source pins and borrowed destinations end before publishing completion.
        // Completed ordinary reads keep their owned bytes until reap.
        if !matches!(self, Self::Owned(_)) {
            *self = Self::default();
        }
    }
    pub(in crate::block) fn write_to_device(
        &self,
        device: &Device,
        source: usize,
        target: u64,
        length: usize,
    ) -> Result<(), Error> {
        match self {
            Self::Owned(bytes) => device.write(
                target,
                bytes
                    .get(source..source.checked_add(length).ok_or(Error::Corrupt)?)
                    .ok_or(Error::Corrupt)?,
            ),
            Self::Window { lease, ranges } => {
                let mut skip = source;
                let mut done = 0;
                for &(offset, count) in ranges {
                    if skip >= count {
                        skip -= count;
                        continue;
                    }
                    let take = (count - skip).min(length - done);
                    device
                        .memory
                        .copy_from_native(
                            lease.memory(),
                            (offset + skip) as u64,
                            target + done as u64,
                            take,
                        )
                        .map_err(|_| Error::Io)?;
                    done += take;
                    skip = 0;
                    if done == length {
                        return Ok(());
                    }
                }
                Err(Error::Corrupt)
            }
            Self::Scatter(_) => Err(Error::Corrupt),
        }
    }
    pub(in crate::block) fn read_from_device(
        &mut self,
        device: &Device,
        source: u64,
        target: usize,
        length: usize,
    ) -> Result<(), Error> {
        match self {
            Self::Owned(bytes) => device.read(
                source,
                bytes
                    .get_mut(target..target.checked_add(length).ok_or(Error::Corrupt)?)
                    .ok_or(Error::Corrupt)?,
            ),
            Self::Scatter(parts) => {
                let mut skip = target;
                let mut done = 0;
                for part in parts {
                    if skip >= part.length {
                        skip -= part.length;
                        continue;
                    }
                    let take = (part.length - skip).min(length - done);
                    // SAFETY: The synchronous submitter retains each disjoint
                    // initialized mutable extent until terminal completion. The
                    // pump owns the sole payload access before publication.
                    let bytes = unsafe {
                        core::slice::from_raw_parts_mut(part.address.as_ptr().add(skip), take)
                    };
                    device.read(source + done as u64, bytes)?;
                    done += take;
                    skip = 0;
                    if done == length {
                        return Ok(());
                    }
                }
                Err(Error::Corrupt)
            }
            Self::Window { .. } => Err(Error::Corrupt),
        }
    }
}
