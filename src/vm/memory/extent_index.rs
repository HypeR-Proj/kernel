// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Dense metadata indices over admitted sparse guest extents. Holes consume no
//! page metadata and can never turn into implicit demand-zero allocations.

const PAGE: u64 = 4096;

pub(super) fn index(extents: impl Iterator<Item = (u64, u64)>, offset: u64) -> Option<usize> {
    let mut ordinal = 0u64;
    for (base, length) in extents {
        if offset >= base && offset - base < length {
            return usize::try_from(ordinal.checked_add((offset - base) / PAGE)?).ok();
        }
        ordinal = ordinal.checked_add(length / PAGE)?;
    }
    None
}

pub(super) fn offset(extents: impl Iterator<Item = (u64, u64)>, index: usize) -> Option<u64> {
    let mut ordinal = u64::try_from(index).ok()?;
    for (base, length) in extents {
        let count = length / PAGE;
        if ordinal < count {
            return base.checked_add(ordinal.checked_mul(PAGE)?);
        }
        ordinal -= count;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn distant_apertures_do_not_consume_dense_metadata() {
        let extents = [(64 << 30, 8192), (1 << 30, 4096)];
        assert_eq!(index(extents.into_iter(), 64 << 30), Some(0));
        assert_eq!(index(extents.into_iter(), (64 << 30) + 8191), Some(1));
        assert_eq!(index(extents.into_iter(), 1 << 30), Some(2));
        assert_eq!(index(extents.into_iter(), 2 << 30), None);
        assert_eq!(offset(extents.into_iter(), 2), Some(1 << 30));
        assert_eq!(offset(extents.into_iter(), 3), None);
    }
    #[test]
    fn boundaries_and_round_trips_preserve_insertion_order() {
        let extents = [(0x8000, 4096), (0x1000, 8192)];
        for page in 0..3 {
            let address = offset(extents.into_iter(), page).unwrap_or(0);
            assert_eq!(index(extents.into_iter(), address), Some(page));
        }
        assert_eq!(index(extents.into_iter(), 0x9000), None);
        assert_eq!(index(extents.into_iter(), 0x3000), None);
    }
}
