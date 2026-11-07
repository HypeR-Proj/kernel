// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Checked borrowed fragments. The owner pins immutable versions through copying.

pub(crate) trait WriteFragments {
    fn count(&self) -> usize;
    fn fragment(&self, index: usize) -> Option<&[u8]>;
}

#[derive(Clone, Copy)]
pub(crate) struct FragmentSource<'a> {
    owner: &'a dyn WriteFragments,
    count: usize,
    total: usize,
    start: usize,
    length: usize,
}

#[derive(Debug, PartialEq)]
pub(crate) enum CopyError<E> {
    Invalid,
    Transfer(E),
}

impl<'a> FragmentSource<'a> {
    pub(crate) fn new(owner: &'a dyn WriteFragments) -> Option<Self> {
        let count = owner.count();
        let maximum = hyper_filesystem::protocol::DATA_BYTES / hyper::mm::PAGE_SIZE as usize;
        if count == 0 || count > maximum {
            return None;
        }
        let mut total = 0usize;
        for index in 0..count {
            let bytes = owner.fragment(index)?;
            if bytes.is_empty() {
                return None;
            }
            total = total.checked_add(bytes.len())?;
        }
        if total > hyper_filesystem::protocol::DATA_BYTES {
            return None;
        }
        Some(Self {
            owner,
            count,
            total,
            start: 0,
            length: total,
        })
    }

    pub(crate) fn len(&self) -> usize {
        self.length
    }

    pub(crate) fn slice(&self, offset: usize, length: usize) -> Option<Self> {
        offset
            .checked_add(length)
            .filter(|end| *end <= self.length)?;
        Some(Self {
            start: self.start.checked_add(offset)?,
            length,
            ..*self
        })
    }

    /// The callback receives offsets relative to the requested output range.
    /// Failure stops copying; callers must not publish a partially filled window.
    pub(crate) fn copy<E>(
        &self,
        offset: usize,
        length: usize,
        mut emit: impl FnMut(usize, &[u8]) -> Result<(), E>,
    ) -> Result<(), CopyError<E>> {
        let source = self.slice(offset, length).ok_or(CopyError::Invalid)?;
        if length == 0 {
            return Ok(());
        }
        let end = source.start.checked_add(length).ok_or(CopyError::Invalid)?;
        let mut position = 0usize;
        let mut copied = 0usize;
        for index in 0..source.count {
            let bytes = source.owner.fragment(index).ok_or(CopyError::Invalid)?;
            if bytes.is_empty() {
                return Err(CopyError::Invalid);
            }
            let next = position
                .checked_add(bytes.len())
                .ok_or(CopyError::Invalid)?;
            if next > source.start && position < end {
                let first = source.start.saturating_sub(position);
                let last = bytes.len().min(end - position);
                let part = bytes.get(first..last).ok_or(CopyError::Invalid)?;
                emit(copied, part).map_err(CopyError::Transfer)?;
                copied += part.len();
            }
            position = next;
        }
        if copied != length || position != source.total {
            return Err(CopyError::Invalid);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Parts<'a>(&'a [&'a [u8]]);
    impl WriteFragments for Parts<'_> {
        fn count(&self) -> usize {
            self.0.len()
        }
        fn fragment(&self, index: usize) -> Option<&[u8]> {
            self.0.get(index).copied()
        }
    }

    #[test]
    fn cross_fragment_slices_and_short_tail_preserve_exact_bytes() -> Result<(), &'static str> {
        let parts = Parts(&[b"abcd", b"EFG", b"hi"]);
        let source = FragmentSource::new(&parts).ok_or("valid fragments")?;
        let slice = source.slice(2, 6).ok_or("valid slice")?;
        let mut output = [0; 4];
        assert_eq!(
            slice.copy(1, 4, |at, bytes| {
                output[at..at + bytes.len()].copy_from_slice(bytes);
                Ok::<_, ()>(())
            }),
            Ok(())
        );
        assert_eq!(&output, b"dEFG");
        assert_eq!(source.len(), 9);
        assert!(source.slice(8, 2).is_none());
        assert!(source.slice(usize::MAX, 2).is_none());
        Ok(())
    }

    #[test]
    fn invalid_range_never_invokes_destination_and_failed_copy_stops() -> Result<(), &'static str> {
        let parts = Parts(&[b"first", b"second", b"third"]);
        let source = FragmentSource::new(&parts).ok_or("valid fragments")?;
        let mut calls = 0;
        assert_eq!(
            source.copy(0, 99, |_, _| {
                calls += 1;
                Ok::<_, ()>(())
            }),
            Err(CopyError::Invalid)
        );
        assert_eq!(calls, 0);
        assert_eq!(
            source.copy(0, source.len(), |_, _| {
                calls += 1;
                if calls == 2 {
                    Err("copy fault")
                } else {
                    Ok(())
                }
            }),
            Err(CopyError::Transfer("copy fault"))
        );
        assert_eq!(calls, 2);
        Ok(())
    }

    #[test]
    fn fragment_and_byte_limits_are_independent() {
        let bytes = alloc::vec![0; hyper_filesystem::protocol::DATA_BYTES + 1];
        assert!(FragmentSource::new(&Parts(&[&bytes])).is_none());
        let parts = alloc::vec![&b"x"[..]; 129];
        assert!(FragmentSource::new(&Parts(&parts)).is_none());
        assert!(FragmentSource::new(&Parts(&[b""])).is_none());
        let full = alloc::vec![&b"01234567"[..]; 128];
        assert!(FragmentSource::new(&Parts(&full)).is_some());
    }
}
