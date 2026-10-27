// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Restricted ELF admission for the kernel's userspace bootstrap program.
//!
//! Ordinary executable formats, interpreters and relocation are userspace
//! policy. This allocation-free parser accepts only a small, self-contained
//! ELF64 image whose load segments need no kernel fixups.

const PAGE: u64 = 4096;
const MAX_SEGMENTS: usize = 8;
pub const MAX_IMAGE_SIZE: u64 = 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Machine {
    Aarch64,
    Riscv64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Header,
    Segment,
    Entry,
}

#[derive(Clone, Copy, Default)]
pub struct Segment {
    pub address: u64,
    pub size: u64,
    pub file_offset: u64,
    pub file_size: u64,
    pub data_offset: u64,
    pub writable: bool,
    pub executable: bool,
}

impl Segment {
    /// Returns the aligned file extent needed to construct this LOAD mapping.
    /// The leading `data_offset` bytes are backing padding, not segment data;
    /// the mapper zeroes them and copies exactly `file_size` bytes after them.
    /// A pure BSS segment needs no file backing, even at a nonzero page offset.
    pub fn backing_bytes<'a>(&self, image: &'a [u8]) -> Result<&'a [u8], Error> {
        if self.file_size == 0 {
            return Ok(&[]);
        }
        let start = usize::try_from(self.file_offset).map_err(|_| Error::Segment)?;
        let length = self
            .file_size
            .checked_add(self.data_offset)
            .ok_or(Error::Segment)?;
        let length = usize::try_from(length).map_err(|_| Error::Segment)?;
        let end = start.checked_add(length).ok_or(Error::Segment)?;
        image.get(start..end).ok_or(Error::Segment)
    }
}

pub struct Image {
    pub machine: Machine,
    pub entry: u64,
    pub size: u64,
    segments: [Segment; MAX_SEGMENTS],
    count: usize,
}

impl Image {
    pub fn parse(bytes: &[u8]) -> Result<Self, Error> {
        if bytes.len() < 64
            || bytes.len() as u64 > MAX_IMAGE_SIZE
            || &bytes[..8] != b"\x7fELF\x02\x01\x01\x3f"
            || bytes[8..16].iter().any(|b| *b != 0)
            || word(bytes, 16, 2)? != 3
            || word(bytes, 20, 4)? != 1
            || word(bytes, 52, 2)? != 64
            || word(bytes, 54, 2)? != 56
        {
            return Err(Error::Header);
        }
        let flags = word(bytes, 48, 4)?;
        let machine = match word(bytes, 18, 2)? {
            183 if flags == 0 => Machine::Aarch64,
            243 if flags & !1 == 4 => Machine::Riscv64,
            _ => return Err(Error::Header),
        };
        let offset = word(bytes, 32, 8)?;
        let count = word(bytes, 56, 2)?;
        if count == 0
            || count > 16
            || offset < 64
            || offset
                .checked_add(count * 56)
                .is_none_or(|end| end > bytes.len() as u64)
        {
            return Err(Error::Header);
        }
        let mut image = Self {
            machine,
            entry: word(bytes, 24, 8)?,
            size: 0,
            segments: [Segment::default(); MAX_SEGMENTS],
            count: 0,
        };
        let mut executable_entry = false;
        for index in 0..count {
            let ph = (offset + index * 56) as usize;
            match word(bytes, ph, 4)? {
                1 => {}
                // Dynamic tags and RELRO describe work owned by the loader,
                // not additional kernel mappings or relocations. PT_INTERP
                // and PT_TLS remain unsupported at this bootstrap boundary.
                0 | 2 | 4 | 6 | 0x6474_e552 => continue,
                0x7000_0003 if machine == Machine::Riscv64 => continue, // PT_RISCV_ATTRIBUTES
                0x6474_e551 if word(bytes, ph + 4, 4)? & 1 == 0 => continue,
                _ => return Err(Error::Segment),
            }
            let flags = word(bytes, ph + 4, 4)?;
            let file = word(bytes, ph + 8, 8)?;
            let address = word(bytes, ph + 16, 8)?;
            let filesz = word(bytes, ph + 32, 8)?;
            let memsz = word(bytes, ph + 40, 8)?;
            let align = word(bytes, ph + 48, 8)?;
            if memsz == 0 && filesz == 0 {
                continue;
            }
            if image.count == MAX_SEGMENTS
                || flags & !7 != 0
                || flags & 4 == 0
                || flags & 3 == 3
                || memsz == 0
                || filesz > memsz
                || align != PAGE
                || file % PAGE != address % PAGE
                || file
                    .checked_add(filesz)
                    .is_none_or(|end| end > bytes.len() as u64)
                || address
                    .checked_add(memsz)
                    .is_none_or(|end| end > MAX_IMAGE_SIZE)
            {
                return Err(Error::Segment);
            }
            let start = address & !(PAGE - 1);
            let end = (address + memsz + PAGE - 1) & !(PAGE - 1);
            if image
                .segments()
                .iter()
                .any(|s| start < s.address + s.size && s.address < end)
            {
                return Err(Error::Segment);
            }
            executable_entry |= flags & 1 != 0
                && address <= image.entry
                && image.entry < address + filesz
                && image.entry.is_multiple_of(4);
            image.segments[image.count] = Segment {
                address: start,
                size: end - start,
                file_offset: file - (address - start),
                file_size: filesz,
                data_offset: address - start,
                writable: flags & 2 != 0,
                executable: flags & 1 != 0,
            };
            image.count += 1;
            image.size = image.size.max(end);
        }
        if !executable_entry || !image.segments().iter().any(|s| s.address == 0) {
            return Err(Error::Entry);
        }
        // The runtime retires the bootstrap as one root-VMAR range. That
        // operation requires complete coverage, including page-rounded padding.
        if image.segments().iter().map(|s| s.size).sum::<u64>() != image.size {
            return Err(Error::Segment);
        }
        Ok(image)
    }

    pub fn segments(&self) -> &[Segment] {
        &self.segments[..self.count]
    }
}

fn word(bytes: &[u8], offset: usize, width: usize) -> Result<u64, Error> {
    let field = bytes.get(offset..offset + width).ok_or(Error::Header)?;
    Ok(field.iter().enumerate().fold(0, |value, (shift, byte)| {
        value | (u64::from(*byte) << (shift * 8))
    }))
}
