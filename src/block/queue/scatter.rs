// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Joined kernel-only reads into privately owned destination pages.

use alloc::vec::Vec;
use core::ptr::NonNull;
use {
    crate::block::BlockAccess, crate::block::BlockQueue, crate::block::Error,
    crate::block::QueueError, crate::block::Range, crate::block::plan,
    crate::block::queue::payload::Destination, crate::block::queue::payload::Payload,
    crate::block::queue_error,
};

impl BlockQueue {
    pub(crate) fn access(&self) -> BlockAccess {
        BlockAccess {
            device: self.device.clone(),
        }
    }
}

impl BlockAccess {
    /// Fill disjoint initialized kernel slices directly from the exchange pool.
    /// No pointer from this path is ever submitted to the I/O VM. The internal
    /// progress queue also prevents public unreaped results from blocking paging.
    #[inline(never)]
    pub(crate) fn read_scatter(
        &self,
        ranges: &[Range],
        output: &mut [&mut [u8]],
    ) -> Result<(), Error> {
        let total = output
            .iter()
            .try_fold(0usize, |sum, part| sum.checked_add(part.len()))
            .ok_or(Error::InvalidRange)?;
        let sectors = self.device.state.with(|state| state.sectors);
        if !plan::validate(plan::Operation::Read, ranges, total, sectors) {
            return Err(Error::InvalidRange);
        }
        let queue = self.queue()?;
        let ticket = loop {
            let mut owned = Vec::new();
            owned
                .try_reserve_exact(ranges.len())
                .map_err(|_| Error::Exhausted)?;
            owned.extend_from_slice(ranges);
            let mut parts = Vec::new();
            parts
                .try_reserve_exact(output.len())
                .map_err(|_| Error::Exhausted)?;
            for part in output.iter_mut().filter(|part| !part.is_empty()) {
                parts.push(Destination {
                    address: NonNull::from(&mut **part).cast(),
                    length: part.len(),
                });
            }
            // The only constructor of a scatter payload retains every mutable
            // borrow through the unconditional completion join below.
            match queue.prepare_payload(0, owned, Some(Payload::Scatter(parts))) {
                Ok(prepared) => break prepared.commit().map_err(queue_error)?,
                Err(QueueError::WouldBlock) => queue
                    .wait_space(total, u64::MAX, || false)
                    .map_err(queue_error)?,
                Err(error) => return Err(queue_error(error)),
            }
        };
        // A failed wait is not cancellation. Resource pressure in wait setup
        // must not release borrowed destinations while the pump still owns them.
        let request = match queue.find(ticket) {
            Ok(request) => request,
            Err(_) => {
                crate::diagnostics::crash::fatal(format_args!("scatter request lost after commit"))
            }
        };
        loop {
            if let Some(completion) = request.completion() {
                let result = completion.result;
                match queue.reap(ticket) {
                    Ok(claim) => {
                        claim.commit();
                    }
                    Err(_) => queue.abandon(ticket),
                }
                return result;
            }
            if queue.wait(ticket, u64::MAX, || false).is_err() {
                let _ = crate::task::scheduler::yield_now();
            }
        }
    }
}
