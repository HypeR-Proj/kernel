// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Fixed-capacity byte storage with non-consuming reads and overflow counting.

use alloc::boxed::Box;

/// Fixed-capacity byte FIFO used by console producers which cannot block.
pub struct ByteRing<const CAPACITY: usize> {
    bytes: [u8; CAPACITY],
    head: usize,
    tail: usize,
    length: usize,
    dropped: u64,
}

impl<const CAPACITY: usize> ByteRing<CAPACITY> {
    pub const fn new() -> Self {
        Self {
            bytes: [0; CAPACITY],
            head: 0,
            tail: 0,
            length: 0,
            dropped: 0,
        }
    }

    /// Fallibly allocates and zero-initializes a ring in its final heap slot.
    ///
    /// This constructor avoids placing the potentially large backing array on
    /// a bounded kernel stack. Every field of `ByteRing` is an integer or byte
    /// array, so the all-zero representation is exactly [`Self::new`].
    pub fn try_boxed() -> Result<Box<Self>, crate::mm::AllocationError> {
        let mut allocation = crate::mm::try_box_uninit::<Self>()?;
        // SAFETY: all-zero is a valid initialized representation for every
        // field, and the unique `MaybeUninit` allocation is written in full
        // before it is converted to `Box<Self>`.
        unsafe {
            allocation.as_mut_ptr().write_bytes(0, 1);
            Ok(allocation.assume_init())
        }
    }

    /// Appends one byte, retaining an overflow count when the FIFO is full.
    pub fn push(&mut self, byte: u8) -> bool {
        if CAPACITY == 0 || self.length == CAPACITY {
            self.dropped = self.dropped.saturating_add(1);
            return false;
        }
        self.bytes[self.head] = byte;
        self.head = (self.head + 1) % CAPACITY;
        self.length += 1;
        true
    }

    /// Removes as many oldest bytes as fit in `output`.
    pub fn pop_into(&mut self, output: &mut [u8]) -> usize {
        let count = self.length.min(output.len());
        for slot in &mut output[..count] {
            *slot = self.bytes[self.tail];
            self.tail = (self.tail + 1) % CAPACITY;
        }
        self.length -= count;
        count
    }

    /// Copies as many oldest bytes as fit without consuming them.
    ///
    /// A caller may use this to prepare a fallible external write and discard
    /// the exact prefix only after that write commits.
    pub fn peek_into(&self, output: &mut [u8]) -> usize {
        let count = self.length.min(output.len());
        let mut index = self.tail;
        for slot in &mut output[..count] {
            *slot = self.bytes[index];
            index = (index + 1) % CAPACITY;
        }
        count
    }

    /// Discards an already-observed prefix.
    ///
    /// Returns `false` without mutation when `count` exceeds the retained
    /// length. This lets transaction owners reject stale or duplicated
    /// commits without partially consuming the FIFO.
    pub fn discard_front(&mut self, count: usize) -> bool {
        if count > self.length {
            return false;
        }
        if CAPACITY != 0 {
            self.tail = (self.tail + count) % CAPACITY;
        }
        self.length -= count;
        true
    }

    pub const fn is_empty(&self) -> bool {
        self.length == 0
    }

    pub const fn remaining_capacity(&self) -> usize {
        CAPACITY - self.length
    }

    /// Discards every retained byte while preserving the allocated storage.
    pub fn clear(&mut self) {
        self.head = 0;
        self.tail = 0;
        self.length = 0;
        self.dropped = 0;
    }

    pub const fn front(&self) -> Option<u8> {
        if self.length == 0 {
            None
        } else {
            Some(self.bytes[self.tail])
        }
    }

    pub fn pop_front(&mut self) -> Option<u8> {
        let byte = self.front()?;
        self.tail = (self.tail + 1) % CAPACITY;
        self.length -= 1;
        Some(byte)
    }

    pub const fn dropped(&self) -> u64 {
        self.dropped
    }
}

impl<const CAPACITY: usize> Default for ByteRing<CAPACITY> {
    fn default() -> Self {
        Self::new()
    }
}
