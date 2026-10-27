// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Generation-qualified ownership of one serialized scheduled reclaim request.

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Request<T> {
    pub(crate) generation: u64,
    pub(crate) target: T,
}

pub(crate) struct RequestState<T> {
    next: u64,
    pending: Option<Request<T>>,
    completed: u64,
}

impl<T: Copy> RequestState<T> {
    pub(crate) const fn new() -> Self {
        Self {
            next: 0,
            pending: None,
            completed: 0,
        }
    }

    pub(crate) fn begin(&mut self, target: T) -> Option<Request<T>> {
        if self.pending.is_some() {
            return None;
        }
        let generation = self.next.checked_add(1)?;
        let request = Request { generation, target };
        self.next = generation;
        self.pending = Some(request);
        Some(request)
    }

    pub(crate) const fn pending(&self) -> Option<Request<T>> {
        self.pending
    }

    pub(crate) fn finish(&mut self, generation: u64) -> bool {
        if self
            .pending
            .is_none_or(|request| request.generation != generation)
        {
            return false;
        }
        self.pending = None;
        self.completed = generation;
        true
    }

    pub(crate) fn completed(&self, generation: u64) -> bool {
        generation != 0 && generation <= self.completed
    }
}

#[cfg(test)]
#[path = "../../../../tests/host/src/cases/cache_reclaim_requests.rs"]
mod tests;
