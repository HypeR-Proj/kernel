// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Pure request geometry and ordering rules, independent of the transport.

#![cfg_attr(test, allow(dead_code))]

pub(crate) const SECTOR_BYTES: usize = 512;
pub(crate) const MAX_RANGES: usize = 16;
pub(crate) const MAX_PAYLOAD: usize = 512 * 1024;
pub(crate) const MAX_REQUESTS: usize = 64;
pub(crate) const MAX_QUEUES: usize = 8;
pub(crate) const MAX_DEVICE_BYTES: usize = 8 * 1024 * 1024;
// The compatibility path services rootfs paging as well as synchronous calls.
// Public completions must not consume the last path to fault in reap buffers.
pub(crate) const MAX_PUBLIC_REQUESTS: usize = MAX_REQUESTS - 1;
pub(crate) const MAX_PUBLIC_BYTES: usize = MAX_DEVICE_BYTES - MAX_PAYLOAD;
pub(crate) const MAX_PIECES: usize = 32;

/// Global admission has a private progress reserve for the rootfs pager. The
/// public budget is a second ceiling, not a deduction from each live sync I/O.
#[derive(Clone, Copy)]
pub(crate) struct Credits {
    pub(crate) requests: usize,
    pub(crate) bytes: usize,
    pub(crate) public_requests: usize,
    pub(crate) public_bytes: usize,
}

impl Credits {
    pub(crate) fn admits(self, internal: bool, bytes: usize) -> bool {
        self.requests < MAX_REQUESTS
            && MAX_DEVICE_BYTES
                .checked_sub(self.bytes)
                .is_some_and(|free| bytes <= free)
            && (internal
                || (self.public_requests < MAX_PUBLIC_REQUESTS
                    && MAX_PUBLIC_BYTES
                        .checked_sub(self.public_bytes)
                        .is_some_and(|free| bytes <= free)))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Range {
    pub(crate) first: u64,
    pub(crate) bytes: usize,
}

impl Range {
    pub(crate) fn end(self) -> Option<u64> {
        self.first.checked_add((self.bytes / SECTOR_BYTES) as u64)
    }

    pub(crate) fn overlaps(self, other: Self) -> bool {
        match (self.end(), other.end()) {
            (Some(end), Some(other_end)) => self.first < other_end && other.first < end,
            _ => true,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Operation {
    Read,
    Write,
    Flush,
}

impl Operation {
    pub(crate) fn decode(value: u32) -> Option<Self> {
        match value {
            0 => Some(Self::Read),
            1 => Some(Self::Write),
            2 => Some(Self::Flush),
            _ => None,
        }
    }

    pub(crate) const fn encode(self) -> u32 {
        match self {
            Self::Read => 0,
            Self::Write => 1,
            Self::Flush => 2,
        }
    }
}

pub(crate) fn validate(operation: Operation, ranges: &[Range], total: usize, sectors: u64) -> bool {
    if operation == Operation::Flush {
        return ranges.is_empty() && total == 0;
    }
    if ranges.is_empty() || ranges.len() > MAX_RANGES || total > MAX_PAYLOAD {
        return false;
    }
    let mut bytes = 0usize;
    for (index, range) in ranges.iter().enumerate() {
        if range.bytes == 0
            || !range.bytes.is_multiple_of(SECTOR_BYTES)
            || range.end().is_none_or(|end| end > sectors)
        {
            return false;
        }
        let Some(next) = bytes.checked_add(range.bytes) else {
            return false;
        };
        bytes = next;
        if ranges[..index]
            .iter()
            .any(|previous| range.overlaps(*previous))
        {
            return false;
        }
    }
    bytes == total
}

/// Reads may run together. Every overlapping pair containing a write is ordered.
pub(crate) fn conflicts(a: Operation, ar: Range, b: Operation, br: Range) -> bool {
    a == Operation::Flush
        || b == Operation::Flush
        || ((a == Operation::Write || b == Operation::Write) && ar.overlaps(br))
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct Cursor {
    range: usize,
    offset: usize,
    payload: usize,
}

impl Cursor {
    pub(crate) fn next(self, ranges: &[Range], maximum: usize) -> Option<(Range, usize)> {
        let range = ranges.get(self.range)?;
        let bytes = (range.bytes - self.offset).min(maximum);
        (bytes != 0).then_some((
            Range {
                first: range.first + (self.offset / SECTOR_BYTES) as u64,
                bytes,
            },
            self.payload,
        ))
    }

    pub(crate) fn advance(&mut self, ranges: &[Range], bytes: usize) {
        self.offset += bytes;
        self.payload += bytes;
        if ranges
            .get(self.range)
            .is_some_and(|range| self.offset == range.bytes)
        {
            self.range += 1;
            self.offset = 0;
        }
    }

    pub(crate) fn exhausted(self, ranges: &[Range]) -> bool {
        self.range == ranges.len()
    }
}

/// An extent of the device's permanently leased bounce memory. Command heads
/// and payload slots are independent: merging consumes bytes, not extra heads.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct DataSpan {
    pub(crate) first: usize,
    pub(crate) count: usize,
}

pub(crate) struct DataSlots {
    slots: usize,
    occupied: u64,
}
impl DataSlots {
    pub(crate) const fn new(slots: usize) -> Self {
        Self { slots, occupied: 0 }
    }
    pub(crate) fn largest_free(&self) -> Option<DataSpan> {
        if self.slots > 63 {
            return None;
        }
        let mut best = DataSpan { first: 0, count: 0 };
        let mut current = best;
        for index in 0..self.slots {
            if self.occupied & (1u64 << index) == 0 {
                if current.count == 0 {
                    current.first = index;
                }
                current.count += 1;
                if current.count > best.count {
                    best = current;
                }
            } else {
                current.count = 0;
            }
        }
        (best.count != 0).then_some(best)
    }
    pub(crate) fn claim(&mut self, span: DataSpan) -> bool {
        if span.count == 0
            || span
                .first
                .checked_add(span.count)
                .is_none_or(|end| end > self.slots)
            || self.slots > 63
        {
            return false;
        }
        let mask = ((1u64 << span.count) - 1) << span.first;
        if self.occupied & mask != 0 {
            return false;
        }
        self.occupied |= mask;
        true
    }
    pub(crate) fn release(&mut self, span: DataSpan) -> bool {
        if span.count == 0
            || span
                .first
                .checked_add(span.count)
                .is_none_or(|end| end > self.slots)
            || self.slots > 63
        {
            return false;
        }
        let mask = ((1u64 << span.count) - 1) << span.first;
        if self.occupied & mask != mask {
            return false;
        }
        self.occupied &= !mask;
        true
    }
}

/// Called for the oldest not-yet-dispatched operation. FIFO supplies pending
/// ordering; this predicate enforces conflicts with already published work.
/// A future priority selector must additionally consider older pending ranges.
pub(crate) fn eligible(
    operation: Operation,
    range: Range,
    active: impl Iterator<Item = (Operation, Range)>,
) -> bool {
    !active
        .into_iter()
        .any(|(other, other_range)| conflicts(operation, range, other, other_range))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_saturation_preserves_one_maximum_pager_request() {
        let full = Credits {
            requests: MAX_PUBLIC_REQUESTS,
            bytes: MAX_PUBLIC_BYTES,
            public_requests: MAX_PUBLIC_REQUESTS,
            public_bytes: MAX_PUBLIC_BYTES,
        };
        assert!(!full.admits(false, 0));
        assert!(!full.admits(false, 512));
        assert!(full.admits(true, MAX_PAYLOAD));
        let with_pager = Credits {
            requests: full.requests + 1,
            bytes: full.bytes + MAX_PAYLOAD,
            ..full
        };
        assert!(!with_pager.admits(true, 0));
        assert!(!with_pager.admits(false, 0));
        // Reaping the pager read reopens exactly its private progress budget.
        assert!(full.admits(true, MAX_PAYLOAD));
    }

    #[test]
    fn exact_byte_admission_does_not_wait_for_a_maximum_batch() {
        let usage = Credits {
            requests: 10,
            bytes: MAX_PUBLIC_BYTES - 64 * 1024,
            public_requests: 10,
            public_bytes: MAX_PUBLIC_BYTES - 64 * 1024,
        };
        assert!(usage.admits(false, 64 * 1024));
        assert!(!usage.admits(false, MAX_PAYLOAD));
        assert!(usage.admits(true, MAX_PAYLOAD));
        assert!(usage.admits(false, 0));
    }

    #[test]
    fn rejects_overlap_overflow_and_nonsectors_before_admission() {
        let a = Range {
            first: 4,
            bytes: 1024,
        };
        assert!(validate(Operation::Write, &[a], 1024, 10));
        assert!(!validate(
            Operation::Write,
            &[
                a,
                Range {
                    first: 5,
                    bytes: 512
                }
            ],
            1536,
            10
        ));
        assert!(!validate(
            Operation::Read,
            &[Range {
                first: u64::MAX,
                bytes: 512
            }],
            512,
            u64::MAX
        ));
        assert!(!validate(
            Operation::Read,
            &[Range { first: 0, bytes: 1 }],
            1,
            10
        ));
        assert!(validate(Operation::Flush, &[], 0, 10));
        assert!(!validate(Operation::Flush, &[a], 1024, 10));
    }

    #[test]
    fn dependency_rules_cover_raw_war_waw_and_flush() {
        let a = Range {
            first: 4,
            bytes: 1024,
        };
        let b = Range {
            first: 5,
            bytes: 1024,
        };
        assert!(conflicts(Operation::Write, a, Operation::Read, b));
        assert!(conflicts(Operation::Read, a, Operation::Write, b));
        assert!(conflicts(Operation::Write, a, Operation::Write, b));
        assert!(!conflicts(Operation::Read, a, Operation::Read, b));
        assert!(!conflicts(
            Operation::Write,
            a,
            Operation::Write,
            Range {
                first: 6,
                bytes: 512
            }
        ));
        assert!(conflicts(Operation::Flush, a, Operation::Read, b));
    }

    #[test]
    fn split_and_merge_keep_payload_order_across_disjoint_ranges() {
        let ranges = [
            Range {
                first: 8,
                bytes: 1024,
            },
            Range {
                first: 100,
                bytes: 512,
            },
        ];
        let mut cursor = Cursor::default();
        assert_eq!(
            cursor.next(&ranges, 512),
            Some((
                Range {
                    first: 8,
                    bytes: 512
                },
                0
            ))
        );
        cursor.advance(&ranges, 512);
        assert_eq!(
            cursor.next(&ranges, 4096),
            Some((
                Range {
                    first: 9,
                    bytes: 512
                },
                512
            ))
        );
        cursor.advance(&ranges, 512);
        assert_eq!(
            cursor.next(&ranges, 4096),
            Some((
                Range {
                    first: 100,
                    bytes: 512
                },
                1024
            ))
        );
        cursor.advance(&ranges, 512);
        assert!(cursor.exhausted(&ranges));
    }

    #[test]
    fn dispatch_predicate_preserves_every_overlapping_dependency() {
        let earlier = Range {
            first: 8,
            bytes: 4096,
        };
        let overlap = Range {
            first: 15,
            bytes: 1024,
        };
        let following = Range {
            first: 16,
            bytes: 1024,
        };
        for old in [Operation::Read, Operation::Write] {
            for new in [Operation::Read, Operation::Write] {
                assert_eq!(
                    eligible(new, overlap, [(old, earlier)].into_iter()),
                    old == Operation::Read && new == Operation::Read,
                );
                assert!(eligible(new, following, [(old, earlier)].into_iter()));
            }
        }
        // Completion of an unrelated command cannot release the dependency.
        let independent = (
            Operation::Read,
            Range {
                first: 100,
                bytes: 512,
            },
        );
        let active = [(Operation::Write, earlier), independent];
        assert!(!eligible(Operation::Read, overlap, active.into_iter()));
        assert!(!eligible(
            Operation::Read,
            overlap,
            active[..1].iter().copied()
        ));
        assert!(eligible(
            Operation::Read,
            overlap,
            active[1..].iter().copied()
        ));
    }

    #[test]
    fn flush_is_a_global_boundary_even_for_disjoint_read_commands() {
        let fence = Range { first: 0, bytes: 0 };
        let distant = Range {
            first: 900,
            bytes: 512,
        };
        for operation in [Operation::Read, Operation::Write, Operation::Flush] {
            assert!(!eligible(
                Operation::Flush,
                fence,
                [(operation, distant)].into_iter()
            ));
            assert!(!eligible(
                operation,
                distant,
                [(Operation::Flush, fence)].into_iter()
            ));
        }
        assert!(eligible(Operation::Flush, fence, core::iter::empty()));
        assert!(eligible(Operation::Write, distant, core::iter::empty()));
    }

    #[test]
    fn one_completed_command_releases_space_without_waiting_for_the_wave() {
        let mut slots = DataSlots::new(4);
        for first in 0..4 {
            assert!(slots.claim(DataSpan { first, count: 1 }));
        }
        assert_eq!(slots.largest_free(), None);
        let completed = DataSpan { first: 1, count: 1 };
        assert!(slots.release(completed));
        assert_eq!(slots.largest_free(), Some(completed));
        assert!(
            slots.claim(completed),
            "refill must not wait for the other three commands"
        );
        assert_eq!(slots.largest_free(), None);
        assert!(
            !slots.claim(completed),
            "two commands cannot own the same DMA storage"
        );
        for first in [3, 0, 2, 1] {
            assert!(slots.release(DataSpan { first, count: 1 }));
        }
        assert_eq!(slots.largest_free(), Some(DataSpan { first: 0, count: 4 }));
    }

    #[test]
    fn data_pool_rejects_overlap_double_release_and_out_of_range_spans() {
        let mut slots = DataSlots::new(4);
        let valid = DataSpan { first: 1, count: 2 };
        assert!(slots.claim(valid));
        assert!(!slots.claim(DataSpan { first: 0, count: 2 }));
        assert!(!slots.release(DataSpan { first: 0, count: 2 }));
        for invalid in [
            DataSpan { first: 0, count: 0 },
            DataSpan { first: 4, count: 1 },
            DataSpan { first: 3, count: 2 },
            DataSpan {
                first: usize::MAX,
                count: 1,
            },
        ] {
            assert!(!slots.claim(invalid));
            assert!(!slots.release(invalid));
        }
        assert!(slots.release(valid));
        assert!(!slots.release(valid));
        assert_eq!(slots.largest_free(), Some(DataSpan { first: 0, count: 4 }));
    }

    #[test]
    fn fragmented_dma_storage_bounds_merges_without_recycling_live_ranges() {
        let mut slots = DataSlots::new(4);
        assert!(slots.claim(DataSpan { first: 1, count: 1 }));
        let largest = slots.largest_free();
        assert_eq!(largest, Some(DataSpan { first: 2, count: 2 }));
        assert!(slots.claim(DataSpan { first: 2, count: 2 }));
        assert_eq!(slots.largest_free(), Some(DataSpan { first: 0, count: 1 }));
        assert!(slots.release(DataSpan { first: 1, count: 1 }));
        assert_eq!(slots.largest_free(), Some(DataSpan { first: 0, count: 2 }));
        assert!(slots.release(DataSpan { first: 2, count: 2 }));
        assert_eq!(slots.largest_free(), Some(DataSpan { first: 0, count: 4 }));
    }

    #[test]
    fn sequential_payload_can_fill_one_512k_command_across_range_boundaries() {
        const CHUNK: usize = 64 * 1024;
        let ranges = core::array::from_fn::<_, 8, _>(|index| Range {
            first: 10 + (index * CHUNK / SECTOR_BYTES) as u64,
            bytes: CHUNK,
        });
        assert!(validate(Operation::Write, &ranges, MAX_PAYLOAD, 2048));
        let mut cursor = Cursor::default();
        let mut bytes = 0;
        for expected in ranges {
            let next = cursor.next(&ranges, MAX_PAYLOAD - bytes);
            assert_eq!(next, Some((expected, bytes)));
            cursor.advance(&ranges, CHUNK);
            bytes += CHUNK;
        }
        assert_eq!(bytes, MAX_PAYLOAD);
        assert!(cursor.exhausted(&ranges));
        assert_eq!(cursor.next(&ranges, MAX_PAYLOAD), None);
    }
}
