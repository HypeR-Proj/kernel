// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Pure geometry for the host stage-1 lower and upper address ranges.
//! Regions use a base and byte count so an upper range may end at 2^64.

pub const PAGE_SIZE: u64 = 4096;
pub const KASLR_ALIGNMENT: u64 = 2 * 1024 * 1024;
const BOOT_STACK_RESERVATION_SIZE: u64 = 2 * 1024 * 1024;
pub const KASLR_WINDOW_SIZE: u64 = 1 << 39;
pub const BOOT_STACK_PAGES: usize = 64;
// Allocation-free firmware discovery retains its bounded scratch data here.
const _: () = assert!(BOOT_STACK_PAGES as u64 * PAGE_SIZE >= 256 * 1024);
const STACK_GUARD_PAGES: u64 = 1;
pub const MAX_RUNTIME_STACK_PAGES: u64 = 64;
const STACK_SLOT_PAGES: u64 = STACK_GUARD_PAGES + MAX_RUNTIME_STACK_PAGES;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AddressRange {
    base: u64,
    size: u64,
}

impl AddressRange {
    pub const fn base(self) -> u64 {
        self.base
    }
    pub const fn size(self) -> u64 {
        self.size
    }

    pub const fn contains(self, address: u64) -> bool {
        address >= self.base && address - self.base < self.size
    }

    /// Tests a nonempty complete span without computing its exclusive end.
    pub const fn contains_span(self, address: u64, size: u64) -> bool {
        self.contains(address) && size != 0 && size <= self.size - (address - self.base)
    }

    pub const fn alias(self, offset: u64, size: u64) -> Option<u64> {
        if size == 0 || offset >= self.size || size > self.size - offset {
            return None;
        }
        self.base.checked_add(offset)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AddressLayout {
    lower: AddressRange,
    upper: AddressRange,
    mmio: AddressRange,
    linear: AddressRange,
    image: AddressRange,
    boot_stack: AddressRange,
    stack_arena: AddressRange,
}

impl AddressLayout {
    pub const fn lower(self) -> AddressRange {
        self.lower
    }
    /// Provisional Native application boundary. The remainder of TTBR0 is
    /// reserved for kernel-managed user mappings, independently of TTBR1.
    pub const fn application_address_limit(self) -> u64 {
        self.lower.size / 2
    }

    pub const fn upper(self) -> AddressRange {
        self.upper
    }
    pub const fn mmio(self) -> AddressRange {
        self.mmio
    }
    pub const fn linear(self) -> AddressRange {
        self.linear
    }
    pub const fn image(self) -> AddressRange {
        self.image
    }

    pub const fn stack_arena(self) -> AddressRange {
        self.stack_arena
    }

    /// Product layout requires at least 42 VA bits; hardware translation
    /// descriptor geometry remains independently defined by the architecture.
    pub const fn new(bits: u32) -> Option<Self> {
        if bits < 42 || bits > 48 {
            return None;
        }
        let size = 1u64 << bits;
        let base = 0u64.wrapping_sub(size);
        let mmio_offset = if size >> 4 < 1 << 39 {
            1 << 39
        } else {
            size >> 4
        };
        let linear_offset = size >> 2;
        let image_offset = size - (1 << 40);
        let boot_offset = image_offset + KASLR_WINDOW_SIZE;
        let arena_offset = boot_offset + BOOT_STACK_RESERVATION_SIZE;
        Some(Self {
            lower: AddressRange { base: 0, size },
            upper: AddressRange { base, size },
            mmio: AddressRange {
                base: base + mmio_offset,
                size: linear_offset - mmio_offset,
            },
            linear: AddressRange {
                base: base + linear_offset,
                size: image_offset - linear_offset,
            },
            image: AddressRange {
                base: base + image_offset,
                size: KASLR_WINDOW_SIZE,
            },
            boot_stack: AddressRange {
                base: base + boot_offset,
                size: BOOT_STACK_RESERVATION_SIZE,
            },
            stack_arena: AddressRange {
                base: base + arena_offset,
                size: size - arena_offset,
            },
        })
    }

    pub const fn boot_stack_bounds(self) -> (u64, u64) {
        let bottom = self.boot_stack.base + PAGE_SIZE;
        (bottom, bottom + BOOT_STACK_PAGES as u64 * PAGE_SIZE)
    }

    pub fn stack_slot(self, slot: u64, pages: u64) -> Option<(u64, u64, u64)> {
        if pages == 0 || pages > MAX_RUNTIME_STACK_PAGES {
            return None;
        }
        let stride = STACK_SLOT_PAGES * PAGE_SIZE;
        let guard = self.stack_arena.alias(slot.checked_mul(stride)?, stride)?;
        let bottom = guard.checked_add(STACK_GUARD_PAGES * PAGE_SIZE)?;
        let top = bottom.checked_add(pages * PAGE_SIZE)?;
        Some((guard, bottom, top))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configured_widths_preserve_disjoint_regions() {
        for bits in 42..=48 {
            let layout =
                AddressLayout::new(bits).unwrap_or_else(|| panic!("expected valid geometry"));
            assert_eq!(layout.lower().size(), 1 << bits);
            assert_eq!(layout.application_address_limit(), 1 << (bits - 1));
            assert_eq!(layout.upper().base(), 0u64.wrapping_sub(1 << bits));
            assert!(layout.upper().contains(u64::MAX));
            assert!(!layout.lower().contains(layout.lower().size()));
            assert!(!layout.upper().contains(layout.upper().base() - 1));
            assert_eq!(
                layout.mmio().base() + layout.mmio().size(),
                layout.linear().base()
            );
            assert_eq!(
                layout.linear().base() + layout.linear().size(),
                layout.image().base()
            );
            assert_eq!(layout.image().base(), 0xffff_ff00_0000_0000);
            assert!(layout.image().base().is_multiple_of(KASLR_ALIGNMENT));
            assert!(layout.image().size().is_multiple_of(KASLR_ALIGNMENT));
            assert_eq!(
                layout.image().base() + layout.image().size(),
                layout.boot_stack.base()
            );
            assert_eq!(
                layout.boot_stack.base() + layout.boot_stack.size(),
                layout.stack_arena().base()
            );
            assert_eq!(layout.mmio().base() % (1 << 39), 0);
        }
        assert!(AddressLayout::new(41).is_none());
        assert!(AddressLayout::new(49).is_none());
        assert_eq!(
            AddressLayout::new(42)
                .unwrap_or_else(|| panic!("expected valid geometry"))
                .mmio()
                .base(),
            0xffff_fc80_0000_0000
        );
        assert_eq!(
            AddressLayout::new(48)
                .unwrap_or_else(|| panic!("expected valid geometry"))
                .linear()
                .base(),
            0xffff_4000_0000_0000
        );
    }

    #[test]
    fn aliases_validate_complete_extent_without_end_overflow() {
        for bits in 42..=48 {
            let layout =
                AddressLayout::new(bits).unwrap_or_else(|| panic!("expected valid geometry"));
            for region in [layout.mmio(), layout.linear(), layout.image()] {
                assert_eq!(
                    region.alias(region.size() - PAGE_SIZE, PAGE_SIZE),
                    Some(region.base() + region.size() - PAGE_SIZE)
                );
                assert_eq!(region.alias(region.size(), 1), None);
                assert_eq!(region.alias(region.size() - PAGE_SIZE, PAGE_SIZE + 1), None);
                assert_eq!(region.alias(u64::MAX, PAGE_SIZE), None);
                assert!(!region.contains_span(region.base(), 0));
                assert!(!region.contains_span(region.base() - 1, 1));
            }
            assert!(layout.upper().contains_span(u64::MAX, 1));
            assert!(!layout.upper().contains_span(u64::MAX, 2));
            assert!(
                layout
                    .upper()
                    .contains_span(layout.upper().base(), layout.upper().size())
            );
        }
    }

    #[test]
    fn stack_slots_fit_their_arena_including_guard_and_unused_tail() {
        let layout = AddressLayout::new(42).unwrap_or_else(|| panic!("expected valid geometry"));
        let (bottom, top) = layout.boot_stack_bounds();
        assert_eq!(bottom, layout.boot_stack.base() + PAGE_SIZE);
        assert!(layout.boot_stack.contains_span(bottom, top - bottom));
        let stride = STACK_SLOT_PAGES * PAGE_SIZE;
        let count = layout.stack_arena().size() / stride;
        let (guard, bottom, top) = layout
            .stack_slot(count - 1, 64)
            .unwrap_or_else(|| panic!("expected valid geometry"));
        assert!(layout.stack_arena().contains_span(guard, stride));
        assert_eq!(bottom - guard, PAGE_SIZE);
        assert_eq!(top - bottom, 64 * PAGE_SIZE);
        assert!(layout.stack_slot(count, 1).is_none());
        assert!(layout.stack_slot(u64::MAX, 1).is_none());
        assert!(layout.stack_slot(0, 0).is_none());
        assert!(layout.stack_slot(0, 65).is_none());
    }
}
