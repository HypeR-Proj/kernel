// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Allocation-free parser for the SVR4 `newc` CPIO format.

use core::str;

const HEADER_SIZE: usize = 110;
const NEWC_MAGIC: &[u8; 6] = b"070701";
const CRC_MAGIC: &[u8; 6] = b"070702";
const TRAILER: &str = "TRAILER!!!";
const FILE_TYPE_MASK: u32 = 0o170_000;
const REGULAR_FILE: u32 = 0o100_000;
const DIRECTORY: u32 = 0o040_000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    ArithmeticOverflow,
    DuplicateEntry,
    InvalidAlignment,
    InvalidChecksum,
    InvalidHex,
    InvalidMagic,
    InvalidName,
    InvalidTrailer,
    Truncated,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EntryKind {
    File,
    Directory,
    Symlink,
    Other,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Entry<'a> {
    name: &'a str,
    data: &'a [u8],
    mode: u32,
    modified_seconds: u32,
    kind: EntryKind,
}

impl<'a> Entry<'a> {
    pub const fn name(&self) -> &'a str {
        self.name
    }

    pub const fn data(&self) -> &'a [u8] {
        self.data
    }

    pub const fn mode(&self) -> u32 {
        self.mode
    }

    pub const fn modified_seconds(&self) -> u32 {
        self.modified_seconds
    }

    pub const fn kind(&self) -> EntryKind {
        self.kind
    }
}

pub struct Archive<'a> {
    bytes: &'a [u8],
}

impl<'a> Archive<'a> {
    pub fn new(bytes: &'a [u8]) -> Result<Self, Error> {
        let archive = Self { bytes };
        let mut iterator = archive.entries();
        for entry in iterator.by_ref() {
            let _ = entry?;
        }
        Ok(archive)
    }

    pub const fn entries(&self) -> Entries<'a> {
        Entries {
            bytes: self.bytes,
            offset: 0,
            finished: false,
        }
    }

    pub fn find_unique(&self, name: &str) -> Result<Option<Entry<'a>>, Error> {
        let mut found = None;
        for entry in self.entries() {
            let entry = entry?;
            if entry.name == name {
                if found.is_some() {
                    return Err(Error::DuplicateEntry);
                }
                found = Some(entry);
            }
        }
        Ok(found)
    }
}

pub struct Entries<'a> {
    bytes: &'a [u8],
    offset: usize,
    finished: bool,
}

impl<'a> Iterator for Entries<'a> {
    type Item = Result<Entry<'a>, Error>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.finished {
            return None;
        }
        match parse_entry(self.bytes, self.offset) {
            Ok(Parsed::Entry(entry, next)) => {
                self.offset = next;
                Some(Ok(entry))
            }
            Ok(Parsed::Trailer(next)) => {
                self.finished = true;
                self.offset = next;
                if self.bytes[next..].iter().all(|byte| *byte == 0) {
                    None
                } else {
                    Some(Err(Error::InvalidTrailer))
                }
            }
            Err(error) => {
                self.finished = true;
                Some(Err(error))
            }
        }
    }
}

enum Parsed<'a> {
    Entry(Entry<'a>, usize),
    Trailer(usize),
}

fn parse_entry(bytes: &[u8], offset: usize) -> Result<Parsed<'_>, Error> {
    let header_end = offset
        .checked_add(HEADER_SIZE)
        .ok_or(Error::ArithmeticOverflow)?;
    let header = bytes.get(offset..header_end).ok_or(Error::Truncated)?;
    let checksum_required = match header.get(..6) {
        Some(magic) if magic == NEWC_MAGIC => false,
        Some(magic) if magic == CRC_MAGIC => true,
        _ => return Err(Error::InvalidMagic),
    };
    let mode = parse_hex(field(header, 14)?)?;
    let modified_seconds = parse_hex(field(header, 46)?)?;
    let file_size =
        usize::try_from(parse_hex(field(header, 54)?)?).map_err(|_| Error::ArithmeticOverflow)?;
    let name_size =
        usize::try_from(parse_hex(field(header, 94)?)?).map_err(|_| Error::ArithmeticOverflow)?;
    let expected_checksum = parse_hex(field(header, 102)?)?;
    if !checksum_required && expected_checksum != 0 {
        return Err(Error::InvalidChecksum);
    }
    if name_size < 2 {
        return Err(Error::InvalidName);
    }
    let name_end = header_end
        .checked_add(name_size)
        .ok_or(Error::ArithmeticOverflow)?;
    let encoded_name = bytes.get(header_end..name_end).ok_or(Error::Truncated)?;
    let name_bytes = encoded_name.strip_suffix(&[0]).ok_or(Error::InvalidName)?;
    if name_bytes.contains(&0) {
        return Err(Error::InvalidName);
    }
    let name = str::from_utf8(name_bytes).map_err(|_| Error::InvalidName)?;
    let data_start = align_four(name_end)?;
    let data_end = data_start
        .checked_add(file_size)
        .ok_or(Error::ArithmeticOverflow)?;
    let data = bytes.get(data_start..data_end).ok_or(Error::Truncated)?;
    if checksum_required
        && data
            .iter()
            .fold(0u32, |sum, byte| sum.wrapping_add(u32::from(*byte)))
            != expected_checksum
    {
        return Err(Error::InvalidChecksum);
    }
    let next = align_four(data_end)?;
    if next > bytes.len() {
        return Err(Error::Truncated);
    }
    if name == TRAILER {
        if file_size != 0 {
            return Err(Error::InvalidTrailer);
        }
        return Ok(Parsed::Trailer(next));
    }
    let kind = match mode & FILE_TYPE_MASK {
        REGULAR_FILE => EntryKind::File,
        DIRECTORY => EntryKind::Directory,
        0o120_000 => EntryKind::Symlink,
        _ => EntryKind::Other,
    };
    Ok(Parsed::Entry(
        Entry {
            name,
            data,
            mode,
            modified_seconds,
            kind,
        },
        next,
    ))
}

fn field(header: &[u8], offset: usize) -> Result<&[u8], Error> {
    header.get(offset..offset + 8).ok_or(Error::Truncated)
}

fn parse_hex(bytes: &[u8]) -> Result<u32, Error> {
    let mut value = 0u32;
    for &byte in bytes {
        let digit = match byte {
            b'0'..=b'9' => u32::from(byte - b'0'),
            b'a'..=b'f' => u32::from(byte - b'a') + 10,
            b'A'..=b'F' => u32::from(byte - b'A') + 10,
            _ => return Err(Error::InvalidHex),
        };
        value = value
            .checked_mul(16)
            .and_then(|current| current.checked_add(digit))
            .ok_or(Error::ArithmeticOverflow)?;
    }
    Ok(value)
}

fn align_four(value: usize) -> Result<usize, Error> {
    value
        .checked_add(3)
        .map(|rounded| rounded & !3)
        .ok_or(Error::InvalidAlignment)
}
