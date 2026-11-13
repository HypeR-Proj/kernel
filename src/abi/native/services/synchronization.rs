// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Synchronization domains and fixed-width shared word waiting.

use crate::capability::{HandleValue, InvocationReceiver};
use crate::process::sync_wait::{Error, Outcome};

pub(crate) struct SyncWaitRequest {
    pub(crate) vmo: HandleValue,
    pub(crate) offset: u64,
    pub(crate) expected: u64,
    pub(crate) owner: u64,
    pub(crate) deadline: u64,
    pub(crate) cancellation: Option<HandleValue>,
}

pub(crate) trait SynchronizationServices {
    fn sync_domain_create(&self) -> Result<HandleValue, Error>;
    fn sync_domain_bind(
        &self,
        domain: &InvocationReceiver,
        vmo: HandleValue,
        offset: u64,
        length: u64,
    ) -> Result<(), Error>;
    fn sync_domain_join(&self, domain: &InvocationReceiver) -> Result<u64, Error>;
    fn sync_domain_owner_query(
        &self,
        domain: &InvocationReceiver,
        owner: u64,
    ) -> Result<u64, Error>;
    fn sync_domain_wait(
        &self,
        domain: &InvocationReceiver,
        request: SyncWaitRequest,
    ) -> Result<Outcome, Error>;
    fn sync_domain_wake(
        &self,
        domain: &InvocationReceiver,
        vmo: HandleValue,
        offset: u64,
        count: u32,
    ) -> Result<u64, Error>;
}
