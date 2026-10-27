// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Immutable firmware facts. Driver binding and dependency policy are userspace.

use super::assigned::service::MatchError;
use crate::vm::service::Error;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU8, Ordering};
use hyper::mm::FallibleArc;
use hyper_drivers::platform::PlatformDevice;

pub(super) struct Catalogue {
    pub(super) nodes: Vec<PlatformDevice>,
    ownership: Vec<AtomicU8>,
}

const OWNED: u8 = 1;
const RESERVED: u8 = 2;

impl Catalogue {
    pub(super) fn new(
        nodes: Vec<PlatformDevice>,
        owned: impl Fn(&PlatformDevice) -> bool,
    ) -> Result<FallibleArc<Self>, ()> {
        let mut reserved = Vec::new();
        reserved.try_reserve_exact(nodes.len()).map_err(|_| ())?;
        reserved.extend((0..nodes.len()).map(|_| AtomicU8::new(0)));
        let catalogue = FallibleArc::try_new(Self {
            nodes,
            ownership: reserved,
        })
        .map_err(|_| ())?;
        catalogue.reserve(owned);
        Ok(catalogue)
    }

    // Boot-only mutation, completed before the registry publishes this owner.
    pub(super) fn reserve(&self, owned: impl Fn(&PlatformDevice) -> bool) {
        // Resolve dependency ownership once per node. The predicate may walk
        // firmware links; invoking it inside the alias scan would be cubic.
        for (index, node) in self.nodes.iter().enumerate() {
            self.ownership[index].store(if owned(node) { OWNED } else { 0 }, Ordering::Relaxed);
        }
        for (index, node) in self.nodes.iter().enumerate() {
            // Protect byte resources and their device pages against firmware
            // aliases. A second node name never grants access to a kernel owner.
            let conflict = self
                .nodes
                .iter()
                .enumerate()
                .filter(|(index, _)| self.ownership[*index].load(Ordering::Relaxed) & OWNED != 0)
                .any(|(_, owner)| {
                    let irq_conflict =
                        shared_interrupts(owner).any(|a| shared_interrupts(node).any(|b| a == b));
                    irq_conflict
                        || owner.mmio_resources().any(|left| {
                            node.mmio_resources().any(|right| {
                                let end = |range: &hyper_drivers::platform::MmioResource| {
                                    range
                                        .start()
                                        .checked_add(range.size())
                                        .and_then(|v| v.checked_add(4095))
                                        .map(|v| v & !4095)
                                };
                                match (end(&left), end(&right)) {
                                    (Some(a), Some(b)) => {
                                        (left.start() & !4095) < b && (right.start() & !4095) < a
                                    }
                                    _ => true,
                                }
                            })
                        })
                });
            let flags = self.ownership[index].load(Ordering::Relaxed);
            self.ownership[index].store(
                flags
                    | if flags & OWNED != 0 || conflict {
                        RESERVED
                    } else {
                        0
                    },
                Ordering::Relaxed,
            );
        }
    }
    pub(super) fn reserved(&self, index: usize) -> bool {
        self.ownership[index].load(Ordering::Relaxed) & RESERVED != 0
    }

    pub(super) fn read(&self, index: u32, field: u32, name: &str) -> Result<Vec<u8>, MatchError> {
        let node = self.nodes.get(index as usize).ok_or(MatchError::Missing)?;
        let mut out = Vec::new();
        let mut append = |bytes: &[u8]| -> Result<(), MatchError> {
            if out
                .len()
                .checked_add(bytes.len())
                .is_none_or(|size| size > 65536)
            {
                return Err(Error::InvalidArgument.into());
            }
            out.try_reserve(bytes.len()).map_err(|_| Error::NoMemory)?;
            out.extend_from_slice(bytes);
            Ok(())
        };
        match field {
            0 => {
                let (interrupt, trigger) = irq(node).unwrap_or((0, 0));
                let mut bytes = [0; 32];
                bytes[..4].copy_from_slice(&u32::from(self.reserved(index as usize)).to_le_bytes());
                bytes[4..8].copy_from_slice(&(node.registers().len() as u32).to_le_bytes());
                bytes[8..12].copy_from_slice(&interrupt.to_le_bytes());
                bytes[12..16].copy_from_slice(&trigger.to_le_bytes());
                append(&bytes)?;
            }
            1 => append(node.path().as_bytes())?,
            4 if node.property(name).is_some() => {
                append(node.property(name).ok_or(MatchError::Missing)?)?;
            }
            2 | 4 if field == 2 || name == "compatible" => {
                for value in node.compatibles() {
                    append(value.as_bytes())?;
                    append(&[0])?;
                }
            }
            3 => {
                for range in node.registers() {
                    append(&range.start().to_le_bytes())?;
                    append(&range.size().to_le_bytes())?;
                }
            }
            4 if name == "interrupts" && !node.interrupt_cells().is_empty() => {
                for cell in node.interrupt_cells() {
                    append(&cell.to_be_bytes())?;
                }
            }
            4 => append(node.property(name).ok_or(MatchError::Missing)?)?,
            5 => {
                for property in node.properties() {
                    append(property.name().as_bytes())?;
                    append(&[0])?;
                }
                if !node.compatibles().is_empty() && node.property("compatible").is_none() {
                    append(b"compatible\0")?;
                }
                if !node.interrupt_cells().is_empty() && node.property("interrupts").is_none() {
                    append(b"interrupts\0")?;
                }
            }
            _ => return Err(Error::InvalidArgument.into()),
        }
        Ok(out)
    }
}

fn irq(node: &PlatformDevice) -> Option<(u32, u32)> {
    let irq = crate::hal::irq::decode_platform(node.interrupt_cells()).ok()?;
    Some((
        irq.interrupt,
        match irq.trigger {
            hyper_platform::PlatformInterruptTrigger::Level => 1,
            hyper_platform::PlatformInterruptTrigger::Edge => 2,
        },
    ))
}

// SMMUv3 has several interrupt descriptors. Protect every host-owned line,
// including an alias whose first interrupt matches a later SMMU descriptor.
fn shared_interrupts(node: &PlatformDevice) -> impl Iterator<Item = u32> + '_ {
    #[cfg(CONFIG_ARCH_AARCH64)]
    let width = 3;
    #[cfg(not(CONFIG_ARCH_AARCH64))]
    let width = node.interrupt_cells().len().max(1);
    node.interrupt_cells()
        .chunks_exact(width)
        .filter_map(|cells| {
            let interrupt = crate::hal::irq::decode_platform(cells).ok()?.interrupt;
            (interrupt >= 32).then_some(interrupt)
        })
}
