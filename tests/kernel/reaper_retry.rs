// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! A real delayed Process retirement must survive a late callout publication.

use hyper::mm::PAGE_SIZE;

use crate::kernel::accounting::{ResourceDomain, ResourceLimits};
use crate::kernel::mm::user_space::{NativeAddressSpace, UserAddress, UserSlice};
use crate::kernel::process::{
    MachineAbi, PreparedProcess, Process, ProcessImage, ProcessNameSnapshot, ProcessObject,
    ProcessPhase, TaskGroup, TerminalReason,
};
use crate::kernel::reaper::retry_test;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Error {
    Construction,
    AddressSpace,
    Group,
    Progress,
    Interleaving,
    Quiescence(super::support::QuiescenceError),
}

pub(super) fn run() -> Result<(), Error> {
    super::support::quiesce_workers().map_err(Error::Quiescence)?;
    let domain =
        ResourceDomain::try_new_root(ResourceLimits::UNLIMITED).map_err(|_| Error::Construction)?;
    let group = TaskGroup::try_new(&domain).map_err(|_| Error::Group)?;
    let result = exercise(&domain, &group);
    group.request_stop().map_err(|_| Error::Group)?;
    group.finish_retirement().map_err(|_| Error::Group)?;
    result?;
    super::support::quiesce_workers().map_err(Error::Quiescence)?;
    crate::pr_info!("HypeR test: late reaper retry publication preserved Process retirement");
    Ok(())
}

fn exercise(domain: &ResourceDomain, group: &TaskGroup) -> Result<(), Error> {
    let process = prepare_process(domain, group)?;
    let mut retained = None;
    let result = (|| {
        // This is the same ownership edge retained by an exported VMAR. It
        // forces the production reaper onto its delayed Process queue.
        retained = Some(
            process
                .address_space_owner()
                .map_err(|_| Error::AddressSpace)?,
        );
        process.request_stop(TerminalReason::Requested);
        progress(|| process.snapshot().phase == ProcessPhase::Retiring)?;
        if !retry_test::arm() {
            return Err(Error::Interleaving);
        }
        progress(|| {
            if retry_test::entered() {
                true
            } else {
                // An unrelated request opens another pass while its first
                // delayed retry is pending. Stop publishing once it enters.
                crate::kernel::reaper::request();
                false
            }
        })?;
        if process.snapshot().phase != ProcessPhase::Retiring {
            return Err(Error::Interleaving);
        }
        drop(retained.take());
        retry_test::release();
        // Observation only: no new request is allowed to rescue the worker
        // after its callback publishes in the controlled late window.
        progress(|| process.snapshot().phase == ProcessPhase::Retired)?;
        if !retry_test::published() {
            return Err(Error::Interleaving);
        }
        Ok(())
    })();
    // Every error path opens the bounded seam and releases the real owner;
    // its one explicit prompt repairs a failed regression before teardown.
    drop(retained);
    retry_test::finish();
    process.request_stop(TerminalReason::Requested);
    progress(|| process.snapshot().phase == ProcessPhase::Retired)?;
    result
}

fn prepare_process(domain: &ResourceDomain, group: &TaskGroup) -> Result<Process, Error> {
    let range =
        UserSlice::new(UserAddress::new(0x40_0000), PAGE_SIZE).map_err(|_| Error::Construction)?;
    // No Thread executes this image: a dormant, stopped Process suffices to
    // exercise address-space reference retirement on the real machine path.
    let image = ProcessImage::try_native(
        MachineAbi::Aarch64,
        range.base(),
        range.end(),
        UserAddress::new(0),
    )
    .map_err(|_| Error::Construction)?;
    let address_space =
        NativeAddressSpace::try_new(domain.clone(), range).map_err(|_| Error::AddressSpace)?;
    let prepared =
        match PreparedProcess::try_new(image, group.clone(), domain.clone(), address_space) {
            Ok(prepared) => prepared,
            Err(failure) => {
                let (_, address_space) = failure.into_parts();
                NativeAddressSpace::retire_unpublished(address_space)
                    .map_err(|_| Error::AddressSpace)?;
                return Err(Error::Construction);
            }
        };
    let object = match ProcessObject::try_service(prepared.process()) {
        Ok(object) => object,
        Err(_) => {
            NativeAddressSpace::retire_unpublished(prepared.abort())
                .map_err(|_| Error::AddressSpace)?;
            return Err(Error::Construction);
        }
    };
    Ok(prepared.publish(object, ProcessNameSnapshot::from_validated("reaper/retry")))
}

fn progress(mut condition: impl FnMut() -> bool) -> Result<(), Error> {
    let done = crate::kernel::task::wait_for_test_progress(
        crate::kernel::task::TEST_PROGRESS_TIMEOUT_NS,
        || Ok::<_, crate::kernel::task::SleepError>(condition()),
    )
    .map_err(|_| Error::Progress)?;
    if done { Ok(()) } else { Err(Error::Progress) }
}
