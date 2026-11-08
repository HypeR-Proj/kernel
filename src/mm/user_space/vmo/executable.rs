// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Unpublished executable copies with bounded instruction-cache maintenance.

use super::{
    ExecutableProvenance, ExecutableVmo, MemoryAccount, PageBackend, VmoError, VmoInner, VmoResult,
    WritableVmo, page_count, page_ref,
};
use hyper::mm::FallibleArc;

/// Maximum number of pages maintained under one caller-supplied CPU pin.
pub(crate) const EXECUTABLE_PUBLICATION_BATCH_PAGES: usize = 16;

/// Owns a fully copied image which cannot be mapped before cache publication.
/// No writable owner or reference escapes preparation. Each successful batch
/// advances monotonically; failure retains an unpublished image for teardown.
pub(crate) struct PreparedExecutableVmo<B: PageBackend, A: MemoryAccount> {
    inner: FallibleArc<VmoInner<B, A>>,
    next_page: usize,
    page_count: usize,
}

impl<B: PageBackend, A: MemoryAccount> WritableVmo<B, A> {
    /// Copies a physically distinct image into the explicit destination account.
    /// Source snapshot admission remains held until all copying completes. The
    /// preparation allocates and may fault; callers must not retain a CPU pin.
    pub(crate) fn prepare_executable_snapshot(
        &self,
        _provenance: &ExecutableProvenance,
        destination_account: A,
    ) -> VmoResult<B, A, PreparedExecutableVmo<B, A>> {
        let snapshot = self.try_snapshot(destination_account)?;
        let page_count = page_count(snapshot.size())?;
        Ok(PreparedExecutableVmo {
            inner: snapshot.inner,
            next_page: 0,
            page_count,
        })
    }
}

impl<B: PageBackend, A: MemoryAccount> PreparedExecutableVmo<B, A> {
    /// Maintains at most sixteen pages without allocating or copying payload.
    /// Returns true once all pages are published. The caller releases its CPU
    /// pin and observes pending rescheduling between batches, even on failure.
    pub(crate) fn publish_next(
        &mut self,
        context: &B::InstructionPublicationContext,
    ) -> VmoResult<B, A, bool> {
        if self.next_page == self.page_count {
            return Ok(true);
        }
        let end = self
            .next_page
            .saturating_add(EXECUTABLE_PUBLICATION_BATCH_PAGES)
            .min(self.page_count);
        let mut page_error = None;
        self.inner
            .backend
            .publish_instruction_pages(context, |visit| {
                for index in self.next_page..end {
                    if page_error.is_some() {
                        break;
                    }
                    match page_ref(&self.inner, index) {
                        Ok(page) => page.with(|owned| visit(&mut owned.page)),
                        Err(error) => page_error = Some(error),
                    }
                }
            })
            .map_err(VmoError::Backend)?;
        if let Some(error) = page_error {
            return Err(error);
        }
        self.next_page = end;
        Ok(self.next_page == self.page_count)
    }

    /// Commits executable ownership only after every batch completed.
    pub(crate) fn finish(self) -> VmoResult<B, A, ExecutableVmo<B, A>> {
        if self.next_page != self.page_count {
            return Err(VmoError::Busy);
        }
        Ok(ExecutableVmo { inner: self.inner })
    }
}
