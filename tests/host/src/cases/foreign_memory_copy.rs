// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Checked copies across a bounded foreign-address-space window.

use hyper::mm::{ForeignCopyError, ForeignMemory, copy_from_foreign, copy_to_foreign};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BackendError {
    InvalidChunk,
}

struct Memory {
    bytes: [u8; 32],
}

impl ForeignMemory for Memory {
    type Error = BackendError;

    fn address_base(&self) -> u64 {
        0x1000
    }

    fn address_size(&self) -> u64 {
        self.bytes.len() as u64
    }

    fn page_size(&self) -> usize {
        8
    }

    fn read_page(
        &mut self,
        page_index: usize,
        page_offset: usize,
        destination: &mut [u8],
    ) -> Result<(), Self::Error> {
        let start = page_index * self.page_size() + page_offset;
        let source = self
            .bytes
            .get(start..start + destination.len())
            .ok_or(BackendError::InvalidChunk)?;
        destination.copy_from_slice(source);
        Ok(())
    }

    fn write_page(
        &mut self,
        page_index: usize,
        page_offset: usize,
        source: &[u8],
    ) -> Result<(), Self::Error> {
        let start = page_index * self.page_size() + page_offset;
        let destination = self
            .bytes
            .get_mut(start..start + source.len())
            .ok_or(BackendError::InvalidChunk)?;
        destination.copy_from_slice(source);
        Ok(())
    }
}

#[test]
fn checks_ranges_and_splits_foreign_copies_at_page_boundaries() {
    let mut memory = Memory { bytes: [0; 32] };
    let source = *b"checked-cross-page-copy";
    crate::require_ok(copy_to_foreign(&mut memory, 0x1005, &source));

    let mut destination = [0; 23];
    crate::require_ok(copy_from_foreign(&mut memory, 0x1005, &mut destination));
    assert_eq!(destination, source);
    assert_eq!(memory.bytes[..5], [0; 5]);
}

#[test]
fn rejects_underflow_overflow_and_out_of_window_copies() {
    let mut memory = Memory { bytes: [0; 32] };
    assert_eq!(
        copy_to_foreign(&mut memory, 0x0fff, &[1]),
        Err(ForeignCopyError::InvalidRange)
    );
    assert_eq!(
        copy_to_foreign(&mut memory, 0x101f, &[1, 2]),
        Err(ForeignCopyError::InvalidRange)
    );
    assert_eq!(
        copy_to_foreign(&mut memory, u64::MAX, &[1, 2]),
        Err(ForeignCopyError::InvalidRange)
    );
}
