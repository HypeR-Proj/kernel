// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Read plans carry media offsets, never pointers. Payload pages stay private to
//! the kernel until complete, while the transport lock stabilizes file layout.

use super::{Error, Operation, Transport, protocol};
use crate::accounting::ResourceDomain;
use crate::fs::primitives::block::BlockDevice;
use {crate::block::BlockAccess, crate::block::Range};
use {crate::fs::vfs::scratch::ScratchBudget, crate::fs::vfs::scratch::ScratchVec};

impl Transport {
    pub(in crate::fs::vfs) fn read_at(
        &mut self,
        path: &str,
        offset: u64,
        output: &mut [u8],
    ) -> Result<usize, Error> {
        self.read_scatter(path, offset, &mut [output])
    }

    #[inline(never)]
    pub(in crate::fs::vfs) fn read_scatter(
        &mut self,
        path: &str,
        offset: u64,
        output: &mut [&mut [u8]],
    ) -> Result<usize, Error> {
        let result = self.read_planned(path, offset, output);
        if result.as_ref().err().is_some_and(|error| error.terminal()) {
            self.health.fail();
        }
        result
    }

    fn read_planned(
        &mut self,
        path: &str,
        offset: u64,
        output: &mut [&mut [u8]],
    ) -> Result<usize, Error> {
        let total = output
            .iter()
            .try_fold(0usize, |n, part| n.checked_add(part.len()))
            .ok_or(Error::InvalidInput)?;
        let mut done = 0usize;
        let sponsor = self.domain.clone();
        let Some(block) = &self.block else {
            return self.read_buffered_scatter(path, offset, output, 0);
        };
        let mut access = block.queue.object().access();
        let volume_bytes = access
            .sector_count()
            .checked_mul(512)
            .ok_or(Error::Corrupt)?;
        while done < total {
            let requested = (total - done).min(protocol::DATA_BYTES);
            let current = offset.checked_add(done as u64).ok_or(Error::InvalidInput)?;
            let size = match self.call(
                Operation::ReadExtents,
                path,
                "",
                current,
                requested as u64,
                [None; 2],
            ) {
                Ok(size) => usize::try_from(size).map_err(|_| Error::Corrupt)?,
                Err(Error::Unsupported) => {
                    let copied = self.read_buffered_scatter(path, current, output, done)?;
                    return Ok(done + copied);
                }
                Err(error) => return Err(error),
            };
            let block = self.block.as_mut().ok_or(Error::Corrupt)?;
            let (plan_bytes, sector) = block.read_scratch.split_at_mut(protocol::EXTENT_PLAN_BYTES);
            let sector: &mut [u8; 512] = sector.try_into().map_err(|_| Error::Corrupt)?;
            if size > plan_bytes.len() {
                return Err(Error::Corrupt);
            }
            self.buffer
                .read_exposed(0, &mut plan_bytes[..size])
                .map_err(|_| Error::Io)?;
            let plan = protocol::ReadExtentPlan::decode(&plan_bytes[..size], requested)
                .ok_or(Error::Corrupt)?;
            // Validate the entire reply before any device or destination side effect.
            if plan.extents().any(|extent| {
                extent
                    .offset
                    .checked_add(extent.length)
                    .is_none_or(|end| end > volume_bytes)
            }) {
                return Err(Error::Corrupt);
            }
            if plan.length() == 0 {
                break;
            }
            let ranges = &mut block.read_ranges;
            let mut count = 0;
            let mut length = 0;
            for extent in plan.extents() {
                let bytes = extent.length as usize;
                if extent.offset.is_multiple_of(512) && bytes.is_multiple_of(512) {
                    ranges[count] = Range {
                        first: extent.offset / 512,
                        bytes,
                    };
                    count += 1;
                    length += bytes;
                    if count < ranges.len() {
                        continue;
                    }
                } else {
                    if count != 0 {
                        access
                            .read_scatter(
                                &ranges[..count],
                                &mut slices(output, done, length, &sponsor)?,
                            )
                            .map_err(block_error)?;
                        done += length;
                        count = 0;
                        length = 0;
                    }
                    read_unaligned(
                        &mut access,
                        sector,
                        extent.offset,
                        output,
                        done,
                        bytes,
                        &sponsor,
                    )?;
                    done += bytes;
                    continue;
                }
                access
                    .read_scatter(
                        &ranges[..count],
                        &mut slices(output, done, length, &sponsor)?,
                    )
                    .map_err(block_error)?;
                done += length;
                count = 0;
                length = 0;
            }
            if count != 0 {
                access
                    .read_scatter(
                        &ranges[..count],
                        &mut slices(output, done, length, &sponsor)?,
                    )
                    .map_err(block_error)?;
                done += length;
            }
        }
        Ok(done)
    }
}

fn slices<'a>(
    parts: &'a mut [&mut [u8]],
    mut skip: usize,
    mut length: usize,
    sponsor: &ResourceDomain,
) -> Result<ScratchVec<&'a mut [u8]>, Error> {
    let mut result = ScratchVec::new(ScratchBudget::new(sponsor));
    result
        .try_reserve_exact(parts.len())
        .map_err(|_| Error::ResourceLimit)?;
    for part in parts {
        if skip >= part.len() {
            skip -= part.len();
            continue;
        }
        let count = (part.len() - skip).min(length);
        result
            .push(&mut part[skip..skip + count])
            .map_err(|_| Error::ResourceLimit)?;
        length -= count;
        skip = 0;
        if length == 0 {
            return Ok(result);
        }
    }
    if length == 0 {
        Ok(result)
    } else {
        Err(Error::Corrupt)
    }
}

// Unaligned bookkeeping ends before the next extent RPC. The sector scratch
// belongs to the mount and remains exclusively borrowed through completion.
#[inline(never)]
fn read_unaligned(
    access: &mut BlockAccess,
    sector: &mut [u8; 512],
    mut disk: u64,
    output: &mut [&mut [u8]],
    mut at: usize,
    mut length: usize,
    sponsor: &ResourceDomain,
) -> Result<(), Error> {
    while length != 0 {
        let inside = (disk % 512) as usize;
        if inside == 0 && length >= 512 {
            let count = (length / 512 * 512).min(protocol::DATA_BYTES);
            access
                .read_scatter(
                    &[Range {
                        first: disk / 512,
                        bytes: count,
                    }],
                    &mut slices(output, at, count, sponsor)?,
                )
                .map_err(block_error)?;
            disk += count as u64;
            at += count;
            length -= count;
        } else {
            access
                .read_scatter(
                    &[Range {
                        first: disk / 512,
                        bytes: 512,
                    }],
                    &mut [sector.as_mut_slice()],
                )
                .map_err(block_error)?;
            let count = (512 - inside).min(length);
            crate::fs::vfs::read_contract::scatter_copy(output, at, count, |copied, part| {
                part.copy_from_slice(&sector[inside + copied..inside + copied + part.len()]);
                Ok::<(), core::convert::Infallible>(())
            })
            .map_err(|_| Error::Corrupt)?;
            disk += count as u64;
            at += count;
            length -= count;
        }
    }
    Ok(())
}

fn block_error(error: crate::fs::primitives::block::Error) -> Error {
    match error {
        crate::fs::primitives::block::Error::Disconnected => Error::Closed,
        crate::fs::primitives::block::Error::InvalidRange
        | crate::fs::primitives::block::Error::Corrupt => Error::Corrupt,
        crate::fs::primitives::block::Error::Exhausted => Error::ResourceLimit,
        _ => Error::Io,
    }
}
