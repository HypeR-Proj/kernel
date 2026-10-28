// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Capability-containment policy for staged Native process construction.

use hyper::abi::native;

/// Explicitly reviewed object kinds which may be retained by `ProcessBuilder`.
///
/// `TransferClass` alone is insufficient: a builder is a persistent capability
/// container and can outlive or cross out of the process that configured it.
/// Lifecycle-bearing Process, Thread, and VMAR objects are excluded. A
/// `TaskGroup` is admitted because a delegated process launcher must name the
/// group of every child it creates; staged startup remains an audited,
/// non-buffered transfer route, and Process retirement closes the delegated
/// handle before retiring its membership edge. VM creation authorities are
/// admitted so the initial supervisor can delegate construction to a fleet
/// manager and move a one-shot lease into an isolated VM runtime. Installed
/// VM and vCPU control objects remain excluded. Guest-memory grants may be
/// delegated to an I/O runtime: they retain backing pages, not a VM lifecycle
/// edge, and preserve the hardware-write lease until their last mapping retires.
/// A `NativeBlock` or derived `BlockQueue` may move into an isolated filesystem
/// worker. Both retain backing and in-flight request leases; staging grants no
/// VM control. A queue keeps its original resource sponsor and bounded credits,
/// and its final active owner closes admission before DMA retirement.
/// `VirtualSerial` is created by
/// its owning runtime and is not a startup transport object. Nested
/// `ProcessBuilder` authority is always forbidden.
pub(crate) struct BuilderStorable;

impl BuilderStorable {
    pub(crate) const fn permits_kind_id(kind: u32) -> bool {
        kind == native::HYPER_NATIVE_OBJECT_EVENT
            || kind == native::HYPER_NATIVE_OBJECT_SYSTEM_INFO
            || kind == native::HYPER_NATIVE_OBJECT_CRASH_VIEW
            || kind == native::HYPER_NATIVE_OBJECT_BYTE_CHANNEL
            || kind == native::HYPER_NATIVE_OBJECT_CAPABILITY_CHANNEL
            || kind == native::HYPER_NATIVE_OBJECT_TASK_GROUP
            || kind == native::HYPER_NATIVE_OBJECT_RESOURCE_DOMAIN
            || kind == native::HYPER_NATIVE_OBJECT_TASK_FACTORY
            || kind == native::HYPER_NATIVE_OBJECT_EXECUTABLE_AUTHORITY
            || kind == native::HYPER_NATIVE_OBJECT_VMO
            || kind == native::HYPER_NATIVE_OBJECT_GUEST_MEMORY
            || kind == native::HYPER_NATIVE_OBJECT_NATIVE_BLOCK
            || kind == native::HYPER_NATIVE_OBJECT_BLOCK_QUEUE
            || kind == native::HYPER_NATIVE_OBJECT_DEVICE_ASSIGNMENT_AUTHORITY
            || kind == native::HYPER_NATIVE_OBJECT_PHYSICAL_DEVICE
            || kind == native::HYPER_NATIVE_OBJECT_CONSOLE
            || kind == native::HYPER_NATIVE_OBJECT_DIRECTORY
            || kind == native::HYPER_NATIVE_OBJECT_FILE
            || kind == native::HYPER_NATIVE_OBJECT_TASK_INSPECTOR
            || kind == native::HYPER_NATIVE_OBJECT_OBJECT_INSPECTOR
            || kind == native::HYPER_NATIVE_OBJECT_MEMORY_INSPECTOR
            || kind == native::HYPER_NATIVE_OBJECT_CPU_INSPECTOR
            || kind == native::HYPER_NATIVE_OBJECT_VIRTUAL_MACHINE_CREATION_AUTHORITY
            || kind == native::HYPER_NATIVE_OBJECT_VIRTUAL_MACHINE_CREATION_LEASE
    }
}
