// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

use super::services::DeferredProcessServices;
use crate::kernel::abi::native::{InspectServices, IpcServices};
use crate::kernel::capability::{HandleError, Rights};
use crate::kernel::inspect::{Error, ObjectInspector};
use crate::kernel::object::diagnostics::DetailRecord;
use crate::kernel::process::{Process, ProcessError, UserThread};

/// Exercise actual syscall service authorization, generation lookup, and peer
/// publication against a real prepared Native process.
pub(crate) fn verify_object_details_for_test(
    process: &Process,
    caller: &UserThread,
) -> Result<(), &'static str> {
    let services = DeferredProcessServices::new(process, caller);
    let domain = process.resource_domain();
    let basic = process
        .create_object(
            ObjectInspector::try_system(&domain).map_err(|_| "basic inspector")?,
            Rights::INSPECT,
        )
        .map_err(|_| "basic inspector publication")?;
    let detailed = process
        .create_object(
            ObjectInspector::try_system(&domain).map_err(|_| "detail inspector")?,
            Rights::INSPECT.union(Rights::INSPECT_DETAILS),
        )
        .map_err(|_| "detail inspector publication")?;
    for pair in [
        services.create_byte_channel().map_err(|_| "byte pair")?,
        services
            .create_capability_channel()
            .map_err(|_| "capability pair")?,
    ] {
        let left = process
            .handle_info(pair[0], Rights::NONE)
            .map_err(|_| "left metadata")?;
        let right = process
            .handle_info(pair[1], Rights::NONE)
            .map_err(|_| "right metadata")?;
        for (owner, target) in [(0, left.koid.get()), (process.koid().get(), pair[0].get())] {
            if !matches!(
                services.read_object_details(basic, owner, target, 0),
                Err(Error::Process(ProcessError::Handle(
                    HandleError::AccessDenied
                )))
            ) {
                return Err("basic inspector admitted details");
            }
            let details = services
                .read_object_details(detailed, owner, target, 0)
                .map_err(|_| "authorized details")?;
            if details.koid != left.koid
                || !matches!(details.details.record, DetailRecord::Channel { peer_koid, local_open: true, peer_open: true, .. } if peer_koid == right.koid.get())
            {
                return Err("channel peer identity");
            }
        }
        process.close_handle(pair[1]).map_err(|_| "close right")?;
        let details = services
            .read_object_details(detailed, 0, left.koid.get(), 0)
            .map_err(|_| "closed peer details")?;
        if !matches!(details.details.record, DetailRecord::Channel { peer_koid, peer_open: false, .. } if peer_koid == right.koid.get())
        {
            return Err("closed peer identity lost or kept open");
        }
        if !matches!(
            services.read_object_details(detailed, process.koid().get(), pair[1].get(), 0),
            Err(Error::NotFound)
        ) {
            return Err("closed handle was resolved");
        }
        process.close_handle(pair[0]).map_err(|_| "close left")?;
    }
    process.close_handle(basic).map_err(|_| "close basic")?;
    process
        .close_handle(detailed)
        .map_err(|_| "close detailed")?;
    crate::pr_info!("HypeR test: Object detail authority and peer identity passed");
    Ok(())
}
