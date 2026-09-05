// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

use hyper::cpu::{CpuIndex, MAX_CPUS, PerCpu};
use hyper::platform::{CpuInfo, CpuList, CpuListError};

#[test]
fn logical_cpu_index_rejects_out_of_capacity_values() {
    assert_eq!(CpuIndex::new(0), Some(CpuIndex::BOOT));
    assert_eq!(
        CpuIndex::new(MAX_CPUS - 1).map(CpuIndex::get),
        Some(MAX_CPUS - 1)
    );
    assert_eq!(CpuIndex::new(MAX_CPUS), None);
    assert_eq!(CpuIndex::new(usize::MAX), None);
}

#[test]
fn per_cpu_storage_requires_a_validated_index() {
    let mut values = PerCpu::new([0usize; MAX_CPUS]);
    let last = super::require_some(CpuIndex::new(MAX_CPUS - 1));

    values[CpuIndex::BOOT] = 7;
    values[last] = 11;

    assert_eq!(values[CpuIndex::BOOT], 7);
    assert_eq!(values[last], 11);
    assert_eq!(values.iter().count(), MAX_CPUS);
}

#[test]
fn firmware_cpu_list_rejects_duplicate_hardware_ids() {
    let mut cpus = CpuList::new();
    assert_eq!(cpus.push(CpuInfo { hardware_id: 7 }), Ok(()));
    assert_eq!(
        cpus.push(CpuInfo { hardware_id: 7 }),
        Err(CpuListError::Duplicate)
    );
    assert_eq!(cpus.as_slice(), &[CpuInfo { hardware_id: 7 }]);
}
