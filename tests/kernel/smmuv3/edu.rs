// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! QEMU EDU endpoint, test-only. Both DMA directions traverse the PCI IOMMU.
//! Contract: <https://www.qemu.org/docs/master/specs/edu.html>

use crate::kernel::device::iommu::KernelEnvironment as E;
use hyper::drivers::iommu::smmuv3::{Environment, Error};
use hyper::drivers::platform::{DriverServices, PlatformDevice};

pub(super) struct Edu {
    config: usize,
    registers: usize,
    pub requester: u16,
}

impl Edu {
    pub(super) fn discover(
        host: &PlatformDevice,
        services: &dyn DriverServices,
    ) -> Result<[Self; 3], Error> {
        if host.property("bus-range") != Some(&[0, 0, 0, 0, 0, 0, 0, 255][..])
            || host.registers().len() != 1
        {
            return Err(Error::Unsupported);
        }
        let config = services
            .map_mmio(host.registers()[0])
            .map_err(|_| Error::Address)?;
        config
            .validate_window(1 << 20, 8)
            .map_err(|_| Error::Address)?;
        let (bus, window) = host.pci_memory().ok_or(Error::Address)?;
        let registers = services.map_mmio(window).map_err(|_| Error::Address)?;
        let aligned_bus = bus.checked_add((1 << 20) - 1).ok_or(Error::Address)? & !((1 << 20) - 1);
        let offset = aligned_bus.checked_sub(bus).ok_or(Error::Address)?;
        if offset + (3 << 20) > window.size() || aligned_bus + (3 << 20) > 1 << 32 {
            return Err(Error::Address);
        }
        let mut devices = [None, None, None];
        let mut count = 0;
        for slot in 1..32 {
            let address = config.virtual_start() + (slot << 15);
            if read32(address) != 0x11e8_1234 {
                continue;
            }
            if count == devices.len() {
                return Err(Error::Unsupported);
            }
            // Only this test owns EDU functions, and EDU has no ATS capability.
            write16(address + 4, 0);
            write32(
                address + 0x10,
                (aligned_bus + ((count as u64) << 20)) as u32,
            );
            write16(address + 4, 6); // memory decoding + bus master
            let device = Self {
                config: address,
                registers: registers.virtual_start() + offset as usize + (count << 20),
                requester: (slot << 3) as u16,
            };
            if read32(device.registers) & 0xffff != 0x00ed {
                return Err(Error::Unsupported);
            }
            devices[count] = Some(device);
            count += 1;
        }
        let [a, b, c] = devices;
        Ok([
            a.ok_or(Error::Unsupported)?,
            b.ok_or(Error::Unsupported)?,
            c.ok_or(Error::Unsupported)?,
        ])
    }

    pub(super) fn read_dma(&mut self, iova: u64) -> Result<(), Error> {
        self.dma(iova, 0x40000, 1, 4096)
    }
    pub(super) fn write_dma(&mut self, iova: u64) -> Result<(), Error> {
        self.dma(0x40000, iova, 3, 4096)
    }

    // Failed QEMU DMA is split into 4-byte transactions; probe one transaction
    // so fault-attribution tests do not intentionally overflow the event queue.
    pub(super) fn probe_read(&mut self, iova: u64) -> Result<(), Error> {
        self.dma(iova, 0x40000, 1, 4)
    }
    pub(super) fn probe_write(&mut self, iova: u64) -> Result<(), Error> {
        self.dma(0x40000, iova, 3, 4)
    }

    fn dma(
        &mut self,
        source: u64,
        destination: u64,
        command: u64,
        length: u64,
    ) -> Result<(), Error> {
        write64(self.registers + 0x80, source);
        write64(self.registers + 0x88, destination);
        write64(self.registers + 0x90, length);
        write64(self.registers + 0x98, command);
        let start = E::now_microseconds();
        while read64(self.registers + 0x98) & 1 != 0 {
            if E::now_microseconds().wrapping_sub(start) >= 2_000_000 {
                return Err(Error::Timeout {
                    register: 0x98,
                    value: 1,
                });
            }
            core::hint::spin_loop();
        }
        E::synchronize();
        Ok(())
    }
}

impl Drop for Edu {
    fn drop(&mut self) {
        write16(self.config + 4, 0);
    }
}

fn read32(address: usize) -> u32 {
    E::synchronize();
    // SAFETY: Callers use aligned registers in the retained ECAM/BAR windows.
    let value = unsafe { core::ptr::read_volatile(address as *const u32) };
    E::synchronize();
    u32::from_le(value)
}
fn write16(address: usize, value: u16) {
    E::synchronize();
    // SAFETY: PCI Command is an aligned 16-bit ECAM register owned by this test.
    unsafe { core::ptr::write_volatile(address as *mut u16, value.to_le()) };
    E::synchronize();
}
fn write32(address: usize, value: u32) {
    E::synchronize();
    // SAFETY: BAR0 is an aligned 32-bit ECAM register owned by this test.
    unsafe { core::ptr::write_volatile(address as *mut u32, value.to_le()) };
    E::synchronize();
}
fn read64(address: usize) -> u64 {
    E::synchronize();
    // SAFETY: Caller uses the aligned 64-bit EDU DMA command register.
    let value = unsafe { core::ptr::read_volatile(address as *const u64) };
    E::synchronize();
    u64::from_le(value)
}
fn write64(address: usize, value: u64) {
    E::synchronize();
    // SAFETY: Caller uses aligned 64-bit EDU DMA registers in its BAR.
    unsafe { core::ptr::write_volatile(address as *mut u64, value.to_le()) };
    E::synchronize();
}
