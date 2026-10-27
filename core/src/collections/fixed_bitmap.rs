// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Fixed-length, non-atomic bits with fallible, exact-capacity word storage.
//! Synchronization and resource admission belong to callers.
use alloc::vec::Vec;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Allocation,
    InvalidRange,
}

/// Fixed-size bitmap whose exact word allocation is admitted before creation.
///
/// An explicit word representation avoids `Vec<bool>`'s implementation-defined
/// capacity rounding and makes retained kernel-byte accounting auditable.
pub struct FixedBitmap {
    words: Vec<usize>,
    bit_count: usize,
}

impl FixedBitmap {
    pub fn try_new(bit_count: usize) -> Result<Self, Error> {
        let word_count = bitmap_word_count(bit_count).ok_or(Error::Allocation)?;
        let mut words = exact_words(word_count)?;
        words.resize(word_count, 0);
        Ok(Self { words, bit_count })
    }

    pub const fn is_empty(&self) -> bool {
        self.bit_count == 0
    }

    pub const fn len(&self) -> usize {
        self.bit_count
    }

    pub fn get(&self, index: usize) -> Option<bool> {
        if index >= self.bit_count {
            return None;
        }
        let word_bits = usize::BITS as usize;
        Some(self.words[index / word_bits] & (1usize << (index % word_bits)) != 0)
    }

    pub fn set(&mut self, index: usize, value: bool) -> Result<(), Error> {
        if index >= self.bit_count {
            return Err(Error::InvalidRange);
        }
        let word_bits = usize::BITS as usize;
        let bit = 1usize << (index % word_bits);
        let word = &mut self.words[index / word_bits];
        if value {
            *word |= bit;
        } else {
            *word &= !bit;
        }
        Ok(())
    }

    pub fn iter(&self) -> impl Iterator<Item = bool> + '_ {
        (0..self.bit_count).map(|index| self.get(index).unwrap_or(false))
    }

    pub fn retained_bytes(&self) -> usize {
        self.words.capacity() * core::mem::size_of::<usize>()
    }
}

/// Required allocator payload, including the final partial word.
pub fn storage_bytes(bit_count: usize) -> Option<usize> {
    bitmap_word_count(bit_count).and_then(|words| words.checked_mul(core::mem::size_of::<usize>()))
}

fn bitmap_word_count(bit_count: usize) -> Option<usize> {
    let word_bits = usize::BITS as usize;
    bit_count
        .checked_add(word_bits - 1)
        .map(|bits| bits / word_bits)
}

fn exact_words(capacity: usize) -> Result<Vec<usize>, Error> {
    let mut words = Vec::new();
    words
        .try_reserve_exact(capacity)
        .map_err(|_| Error::Allocation)?;
    if words.capacity() != capacity {
        return Err(Error::Allocation);
    }
    Ok(words)
}
