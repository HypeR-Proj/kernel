// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Allocation-free admission and interrupt decisions for an assigned transport.

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum VirtioKind {
    Scsi,
    Net,
}

impl VirtioKind {
    pub(crate) const fn from_device_id(id: u32) -> Option<Self> {
        match id {
            8 => Some(Self::Scsi),
            1 => Some(Self::Net),
            _ => None,
        }
    }

    pub(crate) const fn device_id(self) -> u32 {
        match self {
            Self::Scsi => 8,
            Self::Net => 1,
        }
    }

    pub(crate) const fn profile(self) -> u32 {
        use hyper::abi::native as abi;
        match self {
            Self::Scsi => abi::HYPER_NATIVE_DEVICE_PROFILE_VIRTIO_MMIO_SCSI as u32,
            Self::Net => abi::HYPER_NATIVE_DEVICE_PROFILE_VIRTIO_MMIO_NET as u32,
        }
    }
}

pub(crate) const fn transport_profile(profile: u32) -> bool {
    profile == VirtioKind::Scsi.profile()
        || profile == VirtioKind::Net.profile()
        || profile == hyper::abi::native::HYPER_NATIVE_DEVICE_PROFILE_PCI_FUNCTION as u32
}

pub(crate) fn valid_guest_interrupt(irq: u32) -> bool {
    use hyper::abi::native as abi;
    (abi::HYPER_NATIVE_VIRTUAL_PLATFORM_AARCH64_REFERENCE_DEVICE_INTERRUPT_BASE as u32
        ..abi::HYPER_NATIVE_VIRTUAL_PLATFORM_AARCH64_REFERENCE_INTERRUPT_COUNT as u32)
        .contains(&irq)
}

pub(crate) fn valid_guest_interrupt_range(base: u32, count: u32) -> bool {
    count != 0
        && valid_guest_interrupt(base)
        && base
            .checked_add(count - 1)
            .is_some_and(valid_guest_interrupt)
}

pub(crate) fn interrupt_ranges_conflict(
    base: u32,
    count: u32,
    other: u32,
    other_count: u32,
) -> bool {
    if count == 0 || other_count == 0 {
        return false;
    }
    match (base.checked_add(count), other.checked_add(other_count)) {
        (Some(end), Some(other_end)) => base < other_end && other < end,
        _ => true,
    }
}

/// Canonical PCI vendor/device selector, independent of firmware node names.
pub(crate) fn pci_identity(text: &str) -> Option<u32> {
    let bytes = text.as_bytes();
    if bytes.len() != 9 || bytes[4] != b':' {
        return None;
    }
    let word = |digits: &[u8]| {
        digits.iter().try_fold(0u32, |value, digit| {
            let nibble = match digit {
                b'0'..=b'9' => digit - b'0',
                b'a'..=b'f' => digit - b'a' + 10,
                _ => return None,
            };
            Some((value << 4) | u32::from(nibble))
        })
    };
    Some(word(&bytes[..4])? | (word(&bytes[5..])? << 16))
}

/// Compare complete profile-owned apertures, including holes between windows.
pub(crate) fn assignments_conflict(
    (base, size, irq, count): (u64, u64, u32, u32),
    (other_base, other_size, other_irq, other_count): (u64, u64, u32, u32),
) -> bool {
    interrupt_ranges_conflict(irq, count, other_irq, other_count)
        || register_windows_conflict(base, size, other_base, other_size)
}

/// Register access is mediated against exact byte windows, never delegated as
/// a physical-page mapping. Distinct QEMU transports may therefore share a page.
pub(crate) fn register_windows_conflict(
    base: u64,
    size: u64,
    other_base: u64,
    other_size: u64,
) -> bool {
    match (base.checked_add(size), other_base.checked_add(other_size)) {
        (Some(end), Some(other_end)) => base < other_end && other_base < end,
        _ => true,
    }
}

#[derive(Clone, Copy)]
pub(crate) struct Negotiation {
    selector: u32,
    access_platform: bool,
}
impl Negotiation {
    pub(crate) const fn new() -> Self {
        Self {
            selector: 0,
            access_platform: false,
        }
    }
    /// Reject DMA activation without the platform address translation contract.
    pub(crate) fn write(&mut self, offset: usize, value: u32) -> bool {
        match offset {
            0x24 => self.selector = value,
            0x20 if self.selector == 1 => self.access_platform = value & 2 != 0,
            0x70 if value == 0 => *self = Self::new(),
            0x70 if value & 4 != 0 && !self.access_platform => return false,
            _ => {}
        }
        true
    }
}

/// A latched physical IRQ can arrive after the guest already acknowledged it.
/// Masking that empty source would strand the next completion without an ACK.
pub(crate) const fn mask_interrupt(
    active_route: bool,
    status: u32,
    trigger: hyper::hal::interrupt::InterruptTrigger,
) -> bool {
    // Edge sources do not retrigger merely because InterruptStatus stays set.
    // Keep them enabled: each new transition prompts an updated guest level,
    // and concurrent completions remain represented by the durable status.
    !active_route
        || (matches!(trigger, hyper::hal::interrupt::InterruptTrigger::Level) && status != 0)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ExtentError {
    Range,
    Nonresident,
    Noncontiguous,
}
pub(crate) fn contiguous_extent(
    offset: u64,
    length: u64,
    size: u64,
    page_size: u64,
    mut physical: impl FnMut(u64) -> Option<u64>,
) -> Result<u64, ExtentError> {
    if page_size == 0
        || length == 0
        || !offset.is_multiple_of(page_size)
        || !length.is_multiple_of(page_size)
    {
        return Err(ExtentError::Range);
    }
    let end = offset
        .checked_add(length)
        .filter(|end| *end <= size)
        .ok_or(ExtentError::Range)?;
    let first = physical(offset).ok_or(ExtentError::Nonresident)?;
    first.checked_add(length).ok_or(ExtentError::Range)?;
    let mut current = offset;
    while current < end {
        if physical(current).ok_or(ExtentError::Nonresident)? != first + (current - offset) {
            return Err(ExtentError::Noncontiguous);
        }
        current += page_size;
    }
    Ok(first)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SelectionError {
    Missing,
    Ambiguous,
}

/// Selection is independent of current claim state: occupying one match cannot
/// silently redirect a board identity to another controller.
pub(crate) fn unique_match(
    matches: impl IntoIterator<Item = bool>,
) -> Result<usize, SelectionError> {
    let mut selected = None;
    for (index, matches) in matches.into_iter().enumerate() {
        if matches && selected.replace(index).is_some() {
            return Err(SelectionError::Ambiguous);
        }
    }
    selected.ok_or(SelectionError::Missing)
}

/// Byte windows are independent of the page-sized guest trap aperture.
pub(crate) fn register_offset(
    address: usize,
    width: usize,
    window: usize,
    length: u64,
) -> Option<usize> {
    if !matches!(width, 1 | 2 | 4) || !address.is_multiple_of(width) {
        return None;
    }
    let offset = address.checked_sub(window)?;
    let end = offset.checked_add(width)?;
    (u64::try_from(end).ok()? <= length).then_some(offset)
}

pub(crate) fn assignment_aperture(base: u64, size: u64) -> bool {
    size != 0
        && size.is_multiple_of(4096)
        && base.is_multiple_of(4096)
        && (0x0b00_0000..0x0c00_0000).contains(&base)
        && base.checked_add(size).is_some_and(|end| end <= 0x0c00_0000)
}

/// A physical level IRQ's delivery token and userspace notification state.
/// Hardware masking and signal publication are serialized by the owning device;
/// rearm additionally rechecks this state under the IRQ registry lock.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct LevelInterrupt {
    sequence: u64,
    pending: u64,
    readable: bool,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SequenceExhausted;
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct StaleSequence;
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RearmDecision {
    Rearm,
    NoRearm,
}
impl LevelInterrupt {
    pub(crate) const fn new() -> Self {
        Self {
            sequence: 0,
            pending: 0,
            readable: false,
        }
    }
    pub(crate) const fn pending(&self) -> u64 {
        self.pending
    }
    pub(crate) const fn readable(&self) -> bool {
        self.readable
    }
    pub(crate) const fn can_rearm(&self) -> bool {
        self.pending == 0
    }
    /// Coalesced delivery retains the live token. Exhaustion leaves state intact.
    pub(crate) fn deliver(&mut self) -> Result<(), SequenceExhausted> {
        if self.pending == 0 {
            self.sequence = self.sequence.checked_add(1).ok_or(SequenceExhausted)?;
            self.pending = self.sequence;
        }
        self.readable = true;
        Ok(())
    }
    /// Observe an asserted level once without making userspace spin on READABLE.
    /// The token remains valid for deassertion after a guest register write.
    pub(crate) fn complete(
        &mut self,
        sequence: u64,
        asserted: bool,
    ) -> Result<RearmDecision, StaleSequence> {
        if self.pending != sequence {
            return Err(StaleSequence);
        }
        self.readable = false;
        if asserted {
            Ok(RearmDecision::NoRearm)
        } else {
            self.pending = 0;
            Ok(RearmDecision::Rearm)
        }
    }
}

/// Exact ownership exception to the ordinary platform userspace-device window.
/// Legacy kernel-emulated assignments never authorize a userspace overlay.
pub(crate) const fn owns_userspace_aperture(
    userspace: bool,
    assigned_base: u64,
    base: u64,
    length: u64,
) -> bool {
    userspace && base == assigned_base && length == 65536 && base.checked_add(length).is_some()
}

#[cfg(test)]
mod level_interrupt_tests {
    use super::{LevelInterrupt, SequenceExhausted};
    #[test]
    fn token_exhaustion_never_wraps_to_idle_or_republishes_old_identity() {
        let mut irq = LevelInterrupt {
            sequence: u64::MAX,
            pending: 0,
            readable: false,
        };
        let before = irq;
        assert_eq!(irq.deliver(), Err(SequenceExhausted));
        assert_eq!(irq, before);
    }
}
