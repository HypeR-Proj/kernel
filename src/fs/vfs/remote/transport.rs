// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Ordered filesystem RPC. Shared bytes are never interpreted as Rust objects.
//! A timed-out or malformed transaction permanently retires this
//! generation: late server writes cannot become a subsequent request's reply.

use core::sync::atomic::{AtomicBool, Ordering};
use hyper::mm::FallibleArc;
use hyper_filesystem::{
    Timestamp,
    protocol::{
        self, DirectoryBatch, DirectoryCursor, Entry, Error, Operation, Request, ResponseRef,
    },
};
use {
    crate::accounting::ResourceDomain, crate::authority::Rights, crate::capability::HandleValue,
    crate::ipc::ByteChannel, crate::ipc::ByteChannelError, crate::ipc::PreparedByteMessage,
    crate::mm::user_space::DomainAccount, crate::mm::user_space::KernelPageBackend,
    crate::mm::user_space::VmoObject, crate::mm::user_space::WritableMappingLease,
    crate::mm::user_space::WritableVmo, crate::object::KernelObject, crate::object::KernelRef,
    crate::object::KernelService, crate::object::SignalWaitOutcome, crate::object::wait_one,
    crate::process::Process,
};

pub(super) struct Health {
    channel: KernelRef<ByteChannel, KernelService>,
    failed: AtomicBool,
}
impl Health {
    pub(super) fn check(&self) -> Result<(), Error> {
        if self.failed.load(Ordering::Acquire) || self.channel.object().is_closed() {
            Err(Error::Closed)
        } else {
            Ok(())
        }
    }
    pub(super) fn fail(&self) {
        self.failed.store(true, Ordering::Release);
    }
}
pub(in crate::fs::vfs) struct Transport {
    pub(super) health: FallibleArc<Health>,
    buffer: WritableVmo<KernelPageBackend, DomainAccount>,
    block: Option<BlockTransport>,
    mapping: WritableMappingLease<KernelPageBackend, DomainAccount>,
    domain: ResourceDomain,
    sequence: u64,
    read_only: bool,
}
struct BlockTransport {
    queue: KernelRef<crate::block::BlockQueue, KernelService>,
    window: FallibleArc<crate::block::WindowBinding>,
    // The volume mutex serializes the private extent plan, sector bounce
    // buffer and batch descriptors. Allocate once under the mount sponsor,
    // keeping all fixed-size read scratch off nested page-in stacks.
    read_scratch: super::ScratchVec<u8>,
    read_ranges: super::ScratchVec<crate::block::Range>,
}
impl Transport {
    pub(crate) fn publication(&self) -> Option<FallibleArc<crate::block::WindowBinding>> {
        self.block.as_ref().map(|block| block.window.clone())
    }
    // Capability resolution and transport buffers are a construction phase,
    // not live scratch beneath subsequent filesystem mounting and lookup.
    #[inline(never)]
    pub(crate) fn prepare(
        process: &Process,
        channel: HandleValue,
        buffer: HandleValue,
        queue: Option<HandleValue>,
        read_only: bool,
    ) -> Result<Self, super::Error> {
        let channel = process
            .resolve_handle::<ByteChannel>(
                channel,
                Rights::READ.union(Rights::WRITE).union(Rights::WAIT),
            )
            .map_err(|_| super::Error::InvalidInput)?;
        let object = process
            .resolve_handle::<VmoObject>(
                buffer,
                Rights::READ.union(Rights::WRITE).union(Rights::MAP),
            )
            .map_err(|_| super::Error::InvalidInput)?;
        let buffer = object
            .object()
            .writable_clone()
            .ok_or(super::Error::InvalidInput)?;
        if buffer.size() != protocol::DATA_BYTES as u64 {
            return Err(super::Error::InvalidInput);
        }
        let mapping = buffer
            .try_mapping_write_lease()
            .map_err(|_| super::Error::InvalidInput)?;
        buffer
            .populate(0, buffer.size())
            .map_err(|_| super::Error::Allocation)?;
        let block = if let Some(queue) = queue {
            let queue = process
                .resolve_handle::<crate::block::BlockQueue>(
                    queue,
                    Rights::READ.union(Rights::WAIT).union(if read_only {
                        Rights::NONE
                    } else {
                        Rights::WRITE
                    }),
                )
                .map_err(|_| super::Error::InvalidInput)?;
            let window = queue
                .object()
                .bind_window()
                .map_err(|_| super::Error::InvalidInput)?;
            let queue = queue.into_operation_pin().into_filesystem_transport();
            let mut read_scratch =
                super::ScratchVec::new(super::ScratchBudget::new(&process.resource_domain()));
            crate::fs::vfs::scratch::resize_bytes(
                &mut read_scratch,
                protocol::EXTENT_PLAN_BYTES + 512,
            )?;
            let mut read_ranges =
                super::ScratchVec::new(super::ScratchBudget::new(&process.resource_domain()));
            read_ranges.resize(16, crate::block::Range { first: 0, bytes: 0 })?;
            Some(BlockTransport {
                queue,
                window,
                read_scratch,
                read_ranges,
            })
        } else {
            None
        };
        let channel = channel.into_operation_pin().into_filesystem_transport();
        let health = FallibleArc::try_new(Health {
            channel,
            failed: AtomicBool::new(false),
        })?;
        Ok(Self {
            health,
            buffer,
            block,
            mapping,
            domain: process.resource_domain(),
            sequence: 0,
            read_only,
        })
    }
    fn call(
        &mut self,
        operation: Operation,
        path: &str,
        target: &str,
        offset: u64,
        length: u64,
        times: [Option<Timestamp>; 2],
    ) -> Result<u64, Error> {
        self.exchange(
            Request {
                window_token: 0,
                sequence: 0,
                operation,
                path,
                target,
                offset,
                length,
                accessed: times[0],
                modified: times[1],
                cursor: DirectoryCursor::START,
            },
            None,
        )
    }
    fn exchange(
        &mut self,
        request: Request<'_>,
        metadata: Option<&mut Entry>,
    ) -> Result<u64, Error> {
        self.health.check()?;
        if self.read_only
            && matches!(
                request.operation,
                Operation::Write
                    | Operation::Create
                    | Operation::Remove
                    | Operation::Rename
                    | Operation::Resize
                    | Operation::SetMetadata
            )
        {
            return Err(Error::ReadOnly);
        }
        let result = self.transact(request, metadata);
        if result.as_ref().err().is_some_and(|error| error.terminal()) {
            self.health.fail();
        }
        result
    }
    fn transact(
        &mut self,
        mut request: Request<'_>,
        metadata: Option<&mut Entry>,
    ) -> Result<u64, Error> {
        self.sequence = self.sequence.checked_add(1).ok_or(Error::Closed)?;
        request.sequence = self.sequence;
        #[cfg(feature = "kernel-trace")]
        let context = self
            .health
            .channel
            .object()
            .trace_rpc(&[
                hyper::abi::native::HYPER_NATIVE_RPC_TRACE_CLIENT_BEGIN,
                self.sequence,
                request.operation as u64,
                0,
                0,
                0,
            ])
            .ok();
        let result = self.transact_request(request, metadata);
        #[cfg(feature = "kernel-trace")]
        if let Some([generation, _]) = context
            && generation != 0
        {
            let _ = self.health.channel.object().trace_rpc(&[
                hyper::abi::native::HYPER_NATIVE_RPC_TRACE_CLIENT_END,
                self.sequence,
                result.as_ref().err().map_or(0, |error| *error as u64),
                generation,
                0,
                0,
            ]);
        }
        result
    }

    fn transact_request(
        &mut self,
        request: Request<'_>,
        metadata: Option<&mut Entry>,
    ) -> Result<u64, Error> {
        let operation = request.operation;
        let path = request.path;
        let target = request.target;
        let size = protocol::HEADER_BYTES
            .checked_add(path.len())
            .and_then(|n| n.checked_add(target.len()))
            .ok_or(Error::InvalidInput)?;
        let mut message =
            PreparedByteMessage::try_new(&self.domain, size).map_err(|_| Error::Allocation)?;
        request
            .encode(message.bytes_mut())
            .ok_or(Error::InvalidInput)?;
        // Object waits consume absolute nanoseconds, not hardware counter ticks.
        // Prepare a finite deadline before transferring the shared range.
        let deadline_nanoseconds = crate::time::monotonic_nanoseconds()
            .map_err(|_| Error::Closed)?
            .checked_add(60_000_000_000)
            .filter(|deadline| *deadline != hyper::abi::native::HYPER_NATIVE_DEADLINE_INFINITE)
            .ok_or(Error::Closed)?;
        let channel = self.health.channel.object();
        let reservation = channel.prepare_write(&message).map_err(|_| Error::Closed)?;
        reservation.publish(message);
        loop {
            match channel.peek() {
                Ok(info) => {
                    let claim = channel.claim(info).map_err(|_| Error::Closed)?;
                    let response =
                        decode_response(claim.bytes(), self.sequence, operation, metadata);
                    claim.commit().release();
                    return response;
                }
                Err(ByteChannelError::WouldBlock) => {}
                Err(_) => return Err(Error::Closed),
            }
            self.wait_response(deadline_nanoseconds)?;
        }
    }

    // Wait registration follows request publication. Its timer/charge scratch
    // must not remain below channel publication and IRQ-tail scheduling.
    #[inline(never)]
    fn wait_response(&self, deadline_nanoseconds: u64) -> Result<(), Error> {
        let source = self
            .health
            .channel
            .object()
            .signal_source()
            .ok_or(Error::Closed)?;
        match wait_one(
            source,
            &self.domain,
            ByteChannel::READABLE.union(ByteChannel::PEER_CLOSED).bits(),
            deadline_nanoseconds,
            // Publication transfers the shared range to the worker. An
            // app's stop request cannot retire a mount used by other apps.
            // Drain this bounded transaction; the user runner handles stop
            // before the syscall returns. Scheduler cancellation selects one
            // current wait ticket, not every subsequent rearm.
            || false,
        )
        .map_err(|_| Error::Closed)?
        {
            SignalWaitOutcome::Observed(_) | SignalWaitOutcome::Cancelled => Ok(()),
            SignalWaitOutcome::TimedOut => Err(Error::Closed),
        }
    }
    pub(super) fn stat_into(&mut self, path: &str, entry: &mut Entry) -> Result<(), Error> {
        self.exchange(
            Request {
                window_token: 0,
                sequence: 0,
                operation: Operation::Stat,
                path,
                target: "",
                offset: 0,
                length: 0,
                accessed: None,
                modified: None,
                cursor: DirectoryCursor::START,
            },
            Some(entry),
        )
        .map(|_| ())
    }
    /// Directory checkpoints originate in a validated backend batch and stay in
    /// the kernel-owned stream. Native cookies never enter this request.
    pub(super) fn read_directory(
        &mut self,
        path: &str,
        cursor: DirectoryCursor,
        output: &mut [u8; protocol::DIRECTORY_BATCH_BYTES],
    ) -> Result<usize, Error> {
        self.health.check()?;
        let result = (|| {
            let response = self.exchange(
                Request {
                    window_token: 0,
                    sequence: 0,
                    operation: Operation::ReadDirectory,
                    path,
                    target: "",
                    offset: 0,
                    length: output.len() as u64,
                    accessed: None,
                    modified: None,
                    cursor,
                },
                None,
            )?;
            let actual = usize::try_from(response).map_err(|_| Error::Corrupt)?;
            let destination = output.get_mut(..actual).ok_or(Error::Corrupt)?;
            self.buffer
                .read_exposed(0, destination)
                .map_err(|_| Error::Io)?;
            let batch = DirectoryBatch::decode(destination).ok_or(Error::Corrupt)?;
            if batch.start_cursor() != cursor {
                return Err(Error::Corrupt);
            }
            Ok(actual)
        })();
        if result.as_ref().err().is_some_and(|error| error.terminal()) {
            self.health.fail();
        }
        result
    }
    pub(super) fn read_link(&mut self, path: &str, output: &mut [u8]) -> Result<usize, Error> {
        if output.len() > protocol::PATH_BYTES {
            return Err(Error::InvalidInput);
        }
        let response = self.call(
            Operation::ReadLink,
            path,
            "",
            0,
            output.len() as u64,
            [None; 2],
        )?;
        let actual = usize::try_from(response).map_err(|_| Error::Corrupt)?;
        if actual > output.len() {
            self.health.fail();
            return Err(Error::Corrupt);
        }
        self.buffer
            .read_exposed(0, &mut output[..actual])
            .map_err(|_| Error::Io)?;
        Ok(actual)
    }
    /// Batch RPCs by byte capacity, independently of destination page boundaries.
    /// Each reply is copied straight from its shared buffer into private pages;
    /// the scatter visitor allocates neither payload nor pointer metadata.
    fn read_buffered_scatter(
        &mut self,
        path: &str,
        offset: u64,
        output: &mut [&mut [u8]],
        skip: usize,
    ) -> Result<usize, Error> {
        use {
            crate::fs::vfs::read_contract::BatchError, crate::fs::vfs::read_contract::scatter_copy,
        };
        let capacity = output
            .iter()
            .try_fold(0usize, |length, part| length.checked_add(part.len()))
            .and_then(|length| length.checked_sub(skip))
            .ok_or(Error::InvalidInput)?;
        offset
            .checked_add(capacity as u64)
            .ok_or(Error::InvalidInput)?;
        let mut done = 0;
        while done < capacity {
            let requested = (capacity - done).min(protocol::DATA_BYTES);
            let response = self.call(
                Operation::Read,
                path,
                "",
                offset + done as u64,
                requested as u64,
                [None; 2],
            )?;
            let actual = usize::try_from(response).map_err(|_| Error::Corrupt)?;
            if actual > requested {
                return Err(Error::Corrupt);
            }
            scatter_copy(output, skip + done, actual, |source, part| {
                self.buffer
                    .read_exposed(source as u64, part)
                    .map_err(|_| Error::Io)
            })
            .map_err(|error| match error {
                BatchError::Contract(_) => Error::Corrupt,
                BatchError::Transfer(error) => error,
            })?;
            done += actual;
            if actual < requested {
                break;
            }
        }
        Ok(done)
    }
    pub(super) fn write_at(
        &mut self,
        path: &str,
        offset: u64,
        input: &super::super::write_source::WriteSource<'_>,
    ) -> Result<usize, Error> {
        let mut done = 0;
        while done < input.len() {
            self.health.check()?;
            let length = (input.len() - done).min(protocol::DATA_BYTES);
            let window = if let Some(block) = &self.block {
                Some(
                    crate::block::IoWindow::begin(block.window.window(), length, |memory| {
                        input.copy_to_vmo(memory, done, length)
                    })
                    .map_err(|_| Error::Io)?,
                )
            } else {
                // Blockless providers consume the ordinary RPC byte buffer.
                // They receive no queue authority or private page addresses.
                input
                    .copy_to_exposed_vmo(&self.mapping, done, length)
                    .map_err(|_| Error::Io)?;
                None
            };
            let response = self.exchange(
                Request {
                    window_token: window.as_ref().map_or(0, |guard| guard.token()),
                    sequence: 0,
                    operation: Operation::Write,
                    path,
                    target: "",
                    offset: offset.checked_add(done as u64).ok_or(Error::InvalidInput)?,
                    length: length as u64,
                    accessed: None,
                    modified: None,
                    cursor: DirectoryCursor::START,
                },
                None,
            );
            if window.is_some_and(|guard| guard.finish().is_err()) {
                self.health.fail();
                return Err(Error::Corrupt);
            }
            let actual = usize::try_from(response?).map_err(|_| Error::Corrupt)?;
            if actual > length {
                self.health.fail();
                return Err(Error::Corrupt);
            }
            done += actual;
            if actual < length {
                break;
            }
        }
        Ok(done)
    }
    pub(super) fn create(&mut self, path: &str, directory: bool) -> Result<(), Error> {
        self.call(
            Operation::Create,
            path,
            "",
            0,
            u64::from(directory),
            [None; 2],
        )
        .map(|_| ())
    }
    pub(super) fn remove(&mut self, path: &str) -> Result<(), Error> {
        self.call(Operation::Remove, path, "", 0, 0, [None; 2])
            .map(|_| ())
    }
    pub(super) fn rename(&mut self, path: &str, target: &str) -> Result<(), Error> {
        self.call(Operation::Rename, path, target, 0, 0, [None; 2])
            .map(|_| ())
    }
    pub(super) fn resize(&mut self, path: &str, length: u64) -> Result<(), Error> {
        self.call(Operation::Resize, path, "", 0, length, [None; 2])
            .map(|_| ())
    }
    pub(super) fn sync(&mut self) -> Result<(), Error> {
        self.call(Operation::Sync, "", "", 0, 0, [None; 2])
            .map(|_| ())
    }
    pub(super) fn set_times(
        &mut self,
        path: &str,
        accessed: Option<Timestamp>,
        modified: Option<Timestamp>,
    ) -> Result<(), Error> {
        self.call(Operation::SetMetadata, path, "", 0, 0, [accessed, modified])
            .map(|_| ())
    }
}

/// Keep the full metadata reply off the blocking transaction's stack. Most
/// operations only need its scalar result; stat fills the volume's existing
/// metadata scratch directly. All reply fields remain validated in both cases.
#[inline(never)]
fn decode_response(
    bytes: &[u8],
    sequence: u64,
    operation: Operation,
    metadata: Option<&mut Entry>,
) -> Result<u64, Error> {
    let response = ResponseRef::decode(bytes, sequence, operation).ok_or(Error::Corrupt)?;
    let actual = response.result()?;
    if let Some(metadata) = metadata {
        response.entry_into(metadata);
    }
    Ok(actual)
}

#[path = "transfer.rs"]
mod transfer;
