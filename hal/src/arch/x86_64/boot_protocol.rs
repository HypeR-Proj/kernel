// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

use core::ptr::{read_unaligned, read_volatile};

use hyper::platform::{PhysicalRange, chosen::CommandLine};

const RAMDISK_IMAGE_OFFSET: usize = 0x218;
const RAMDISK_SIZE_OFFSET: usize = 0x21c;
const COMMAND_LINE_OFFSET: usize = 0x228;
const SETUP_DATA_OFFSET: usize = 0x250;
const SETUP_DTB: u32 = 2;
const HEADER_SIZE: usize = 16;
const MAX_SETUP_DATA_NODES: usize = 32;
const MAX_COMMAND_LINE: usize = 4096;
const SETUP_IMAGE_SIZE: usize = 5 * 512;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    InvalidCommandLine,
    InvalidDtb,
    InvalidRamdisk,
    InvalidSetupData,
    MissingDtb,
}

pub struct Inputs {
    pub dtb_address: usize,
    pub command_line: Option<CommandLine>,
    pub initial_ramdisk: Option<PhysicalRange>,
}

pub unsafe fn parse(boot_params: usize) -> Result<Inputs, Error> {
    if boot_params == 0 {
        return Err(Error::InvalidSetupData);
    }
    // SAFETY: The parse contract covers the complete boot-parameter record.
    let command_line_address = unsafe { read_u32(boot_params + COMMAND_LINE_OFFSET) } as usize;
    let command_line = if command_line_address == 0 {
        None
    } else {
        // SAFETY: Firmware supplied this pointer within the retained boot payload.
        Some(unsafe { read_command_line(command_line_address)? })
    };
    // SAFETY: The parse contract covers both ramdisk fields.
    let ramdisk_start = u64::from(unsafe { read_u32(boot_params + RAMDISK_IMAGE_OFFSET) });
    // SAFETY: The parse contract covers both ramdisk fields.
    let ramdisk_size = u64::from(unsafe { read_u32(boot_params + RAMDISK_SIZE_OFFSET) });
    let initial_ramdisk = if ramdisk_start == 0 && ramdisk_size == 0 {
        None
    } else {
        Some(PhysicalRange::new(ramdisk_start, ramdisk_size).ok_or(Error::InvalidRamdisk)?)
    };
    // SAFETY: The parse contract covers the setup-data pointer field.
    let setup_data = unsafe { read_u64(boot_params + SETUP_DATA_OFFSET) } as usize;
    // SAFETY: Firmware retains the linked setup-data records during parsing.
    let dtb_address = unsafe { find_dtb(setup_data)? };
    Ok(Inputs {
        dtb_address: dtb_address.ok_or(Error::MissingDtb)?,
        command_line,
        initial_ramdisk,
    })
}

unsafe fn find_dtb(setup_data: usize) -> Result<Option<usize>, Error> {
    if setup_data == 0 {
        return Ok(None);
    }
    // SAFETY: The caller guarantees a retained setup-data chain.
    if let Some(address) = unsafe { walk_setup_data(setup_data)? } {
        return Ok(Some(address));
    }
    // QEMU's direct Linux loader reports setup_data relative to the complete
    // bzImage while loading the protected payload with its setup sectors
    // removed. Linux's real-mode setup normally performs this normalization;
    // this image supplies its own setup stage, so retry the normalized address.
    match setup_data.checked_sub(SETUP_IMAGE_SIZE) {
        // SAFETY: This is the documented QEMU payload normalization of the same chain.
        Some(normalized) => unsafe { walk_setup_data(normalized) },
        None => Ok(None),
    }
}

unsafe fn walk_setup_data(mut node: usize) -> Result<Option<usize>, Error> {
    for _ in 0..MAX_SETUP_DATA_NODES {
        if node == 0 {
            return Ok(None);
        }
        // SAFETY: The walk contract guarantees a readable setup-data header.
        let next = unsafe { read_u64(node) } as usize;
        // SAFETY: The walk contract guarantees a readable setup-data header.
        let kind = unsafe { read_u32(node + 8) };
        // SAFETY: The walk contract guarantees a readable setup-data header.
        let length = unsafe { read_u32(node + 12) } as usize;
        if kind == SETUP_DTB {
            if length < 40 {
                return Err(Error::InvalidDtb);
            }
            return node
                .checked_add(HEADER_SIZE)
                .map(Some)
                .ok_or(Error::InvalidSetupData);
        }
        node = next;
    }
    Err(Error::InvalidSetupData)
}

unsafe fn read_command_line(address: usize) -> Result<CommandLine, Error> {
    let mut bytes = [0_u8; MAX_COMMAND_LINE];
    let mut length = 0;
    while length < bytes.len() {
        // SAFETY: The caller guarantees MAX_COMMAND_LINE readable bytes at `address`.
        let byte = unsafe { read_volatile((address + length) as *const u8) };
        if byte == 0 {
            let value =
                core::str::from_utf8(&bytes[..length]).map_err(|_| Error::InvalidCommandLine)?;
            return CommandLine::parse(value).map_err(|_| Error::InvalidCommandLine);
        }
        bytes[length] = byte;
        length += 1;
    }
    Err(Error::InvalidCommandLine)
}

unsafe fn read_u32(address: usize) -> u32 {
    // SAFETY: The caller guarantees four readable bytes; unaligned access is intentional.
    unsafe { read_unaligned(address as *const u32) }
}

unsafe fn read_u64(address: usize) -> u64 {
    // SAFETY: The caller guarantees eight readable bytes; unaligned access is intentional.
    unsafe { read_unaligned(address as *const u64) }
}
