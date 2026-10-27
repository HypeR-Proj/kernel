// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Device-local, non-consuming fault evidence survives capability handoff.

use hyper::{abi::native as abi, mm::FallibleArc, sync::InterruptSpinLock};
use {
    crate::accounting::CommittedCharge, crate::accounting::ResourceAmount,
    crate::accounting::ResourceDomain, crate::accounting::ResourceKind, crate::object::SignalMask,
    crate::object::SignalState,
};

pub(crate) struct Events {
    pub(crate) signals: SignalState,
    first:
        InterruptSpinLock<Option<abi::HyperNativeDeviceDmaFaultInfo>, crate::hal::irq::LocalMask>,
    _charge: CommittedCharge,
}

impl Events {
    pub(crate) fn new(domain: &ResourceDomain) -> Result<FallibleArc<Self>, super::Error> {
        let charge = domain
            .reserve(ResourceAmount::ZERO.with(
                ResourceKind::KernelMemoryBytes,
                FallibleArc::<Self>::allocation_size() as u64,
            ))
            .map_err(|_| super::Error::Resource)?
            .commit();
        FallibleArc::try_new(Self {
            signals: SignalState::new(),
            first: InterruptSpinLock::new(None),
            _charge: charge,
        })
        .map_err(|_| super::Error::Resource)
    }

    /// Only the first containment report is retained. Neither inspection nor a
    /// new userspace owner acknowledges it or restores DMA admission.
    pub(crate) fn publish(&self, record: abi::HyperNativeDeviceDmaFaultInfo) {
        let first = self.first.with(|slot| {
            if slot.is_some() {
                false
            } else {
                *slot = Some(record);
                true
            }
        });
        if first
            && self
                .signals
                .update(
                    SignalMask::EMPTY,
                    SignalMask::from_trusted_bits(
                        abi::HYPER_NATIVE_SIGNAL_PHYSICAL_DEVICE_DMA_FAULT,
                    ),
                )
                .is_err()
        {
            crate::diagnostics::crash::fatal(format_args!(
                "physical DMA fault signal sequence exhausted"
            ));
        }
    }

    pub(crate) fn snapshot(&self) -> [u8; 40] {
        let mut bytes = [0; 40];
        if let Some(record) = self.first.with(|slot| *slot) {
            bytes[0..8].copy_from_slice(&record.timestamp_ns.to_le_bytes());
            bytes[8..16].copy_from_slice(&record.address.to_le_bytes());
            for (index, value) in [
                record.stream_id,
                record.source,
                record.scope,
                record.code,
                record.flags,
                0,
            ]
            .into_iter()
            .enumerate()
            {
                bytes[16 + index * 4..20 + index * 4].copy_from_slice(&value.to_le_bytes());
            }
        }
        bytes
    }
}
