// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Fault routing to the existing physical-device capability, never a process ID.

use alloc::vec::Vec;
use {crate::device::assigned::events::Events, crate::sync::Mutex};
use {
    hyper::abi::native as abi, hyper::mm::FallibleArc, hyper::mm::WeakFallibleArc,
    hyper_drivers::iommu::smmuv3,
};

pub(super) struct Observers(Mutex<Vec<(u32, WeakFallibleArc<Events>)>>);

impl Observers {
    pub(super) const fn new() -> Self {
        Self(Mutex::new(Vec::new()))
    }

    pub(super) fn register(
        &self,
        stream: u32,
        events: &FallibleArc<Events>,
    ) -> Result<(), smmuv3::Error> {
        let mut entries = self.0.lock().unwrap_or_else(|error| {
            crate::diagnostics::crash::fatal(format_args!("SMMU observer lock failed: {error:?}"))
        });
        if let Some(entry) = entries.iter_mut().find(|entry| entry.0 == stream) {
            entry.1 = events.downgrade();
        } else {
            entries
                .try_reserve(1)
                .map_err(|_| smmuv3::Error::Allocation)?;
            entries.push((stream, events.downgrade()));
        }
        Ok(())
    }

    /// No allocation or controller/device locks across notification. Entries
    /// are append-only and reused by SID; weak references never retain devices
    /// or VM RAM. Registration precedes hardware activation.
    pub(super) fn publish(
        &self,
        stream: Option<u32>,
        mut record: abi::HyperNativeDeviceDmaFaultInfo,
    ) {
        let mut index = 0;
        loop {
            let next = {
                let entries = self.0.lock().unwrap_or_else(|error| {
                    crate::diagnostics::crash::fatal(format_args!(
                        "SMMU observer lock failed: {error:?}"
                    ))
                });
                entries.get(index).map(|(sid, weak)| (*sid, weak.upgrade()))
            };
            let Some((sid, events)) = next else { break };
            index += 1;
            if stream.is_none_or(|stream| stream == sid)
                && let Some(events) = events
            {
                record.stream_id = sid;
                events.publish(record);
            }
        }
    }
}

pub(super) fn stream_record(event: smmuv3::Event) -> abi::HyperNativeDeviceDmaFaultInfo {
    // Only translation/access faults give the address its DMA-address meaning.
    let valid = matches!(event.kind(), 0x10..=0x13);
    abi::HyperNativeDeviceDmaFaultInfo {
        timestamp_ns: crate::time::monotonic_microseconds().saturating_mul(1000),
        address: if valid { event.address() } else { 0 },
        stream_id: event.stream(),
        source: abi::HYPER_NATIVE_DMA_FAULT_SOURCE_SMMUV3 as u32,
        scope: abi::HYPER_NATIVE_DMA_FAULT_SCOPE_STREAM as u32,
        code: event.kind().into(),
        flags: if valid {
            (abi::HYPER_NATIVE_DMA_FAULT_ADDRESS_VALID
                | if event.read() {
                    abi::HYPER_NATIVE_DMA_FAULT_READ
                } else {
                    abi::HYPER_NATIVE_DMA_FAULT_WRITE
                }) as u32
        } else {
            0
        },
        reserved: 0,
    }
}

pub(super) fn controller_record(failure: smmuv3::Failure) -> abi::HyperNativeDeviceDmaFaultInfo {
    abi::HyperNativeDeviceDmaFaultInfo {
        timestamp_ns: crate::time::monotonic_microseconds().saturating_mul(1000),
        address: 0,
        stream_id: 0,
        source: abi::HYPER_NATIVE_DMA_FAULT_SOURCE_SMMUV3 as u32,
        scope: abi::HYPER_NATIVE_DMA_FAULT_SCOPE_CONTROLLER as u32,
        code: failure.global_errors,
        flags: 0,
        reserved: 0,
    }
}
