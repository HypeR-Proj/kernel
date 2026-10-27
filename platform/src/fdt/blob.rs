// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Bounded FDT blob and structure-token validation.
//!
//! This module owns byte bounds, header offsets, and token framing. It does not
//! interpret node resources or invoke device-discovery policy.

use core::str;

use super::Error;

pub(super) const HEADER_SIZE: usize = 40;
const FDT_MAGIC: u32 = 0xd00d_feed;
const FDT_BEGIN_NODE: u32 = 1;
const FDT_END_NODE: u32 = 2;
const FDT_PROP: u32 = 3;
const FDT_NOP: u32 = 4;
const FDT_END: u32 = 9;
const MAX_DTB_SIZE: usize = 2 * 1024 * 1024;

#[derive(Clone, Copy)]
struct Header {
    total_size: usize,
    structure_offset: usize,
    strings_offset: usize,
    reservation_offset: usize,
    strings_size: usize,
    structure_size: usize,
}

/// A complete FDT whose header-described regions are bounded by `bytes`.
pub(super) struct Blob<'a> {
    bytes: &'a [u8],
    structure: &'a [u8],
    strings: &'a [u8],
    reservation_offset: usize,
    total_size: usize,
}

impl<'a> Blob<'a> {
    pub(super) fn from_bytes(bytes: &'a [u8]) -> Result<Self, Error> {
        let header = parse_header(bytes)?;
        let bytes = bytes.get(..header.total_size).ok_or(Error::Truncated)?;
        let structure = region(bytes, header.structure_offset, header.structure_size)?;
        let strings = region(bytes, header.strings_offset, header.strings_size)?;
        Ok(Self {
            bytes,
            structure,
            strings,
            reservation_offset: header.reservation_offset,
            total_size: header.total_size,
        })
    }

    pub(super) fn total_size(&self) -> usize {
        self.total_size
    }

    pub(super) fn reservations(&self) -> ReservationReader<'a> {
        ReservationReader {
            blob: self.bytes,
            cursor: self.reservation_offset,
        }
    }

    pub(super) fn tokens(&self) -> TokenReader<'a> {
        TokenReader {
            structure: self.structure,
            cursor: 0,
        }
    }

    pub(super) fn property_name(&self, offset: usize) -> Result<&'a str, Error> {
        take_string_at(self.strings, offset)
    }
}

/// Bounds-checks entries in the header reservation map.
pub(super) struct ReservationReader<'a> {
    blob: &'a [u8],
    cursor: usize,
}

impl ReservationReader<'_> {
    pub(super) fn next(&mut self) -> Result<Option<(u64, u64)>, Error> {
        let address = read_u64(self.blob, self.cursor)?;
        let size_offset = self.cursor.checked_add(8).ok_or(Error::Truncated)?;
        let size = read_u64(self.blob, size_offset)?;
        self.cursor = self.cursor.checked_add(16).ok_or(Error::Truncated)?;
        if address == 0 && size == 0 {
            Ok(None)
        } else {
            Ok(Some((address, size)))
        }
    }
}

/// One fully framed structure-block event.
pub(super) enum Token<'a> {
    BeginNode(&'a str),
    EndNode,
    Property { name_offset: usize, value: &'a [u8] },
    Nop,
    End,
}

/// Bounds-checks complete tokens before exposing them to the resource walker.
pub(super) struct TokenReader<'a> {
    structure: &'a [u8],
    cursor: usize,
}

impl<'a> TokenReader<'a> {
    pub(super) fn next(&mut self) -> Result<Token<'a>, Error> {
        match take_u32(self.structure, &mut self.cursor)? {
            FDT_BEGIN_NODE => {
                let name = take_c_string(self.structure, &mut self.cursor)?;
                align_cursor(&mut self.cursor, self.structure.len())?;
                Ok(Token::BeginNode(name))
            }
            FDT_END_NODE => Ok(Token::EndNode),
            FDT_PROP => {
                let length = take_u32(self.structure, &mut self.cursor)? as usize;
                let name_offset = take_u32(self.structure, &mut self.cursor)? as usize;
                let value = take_bytes(self.structure, &mut self.cursor, length)?;
                align_cursor(&mut self.cursor, self.structure.len())?;
                Ok(Token::Property { name_offset, value })
            }
            FDT_NOP => Ok(Token::Nop),
            FDT_END => Ok(Token::End),
            _ => Err(Error::BadStructure),
        }
    }
}

/// Reads the total size after validating the fixed header.
///
/// This is used before constructing the complete slice for a firmware pointer.
pub(super) fn total_size(header_bytes: &[u8]) -> Result<usize, Error> {
    parse_header(header_bytes).map(|header| header.total_size)
}

fn parse_header(bytes: &[u8]) -> Result<Header, Error> {
    if bytes.len() < HEADER_SIZE {
        return Err(Error::Truncated);
    }
    if read_u32(bytes, 0)? != FDT_MAGIC {
        return Err(Error::BadMagic);
    }

    let header = Header {
        total_size: read_u32(bytes, 4)? as usize,
        structure_offset: read_u32(bytes, 8)? as usize,
        strings_offset: read_u32(bytes, 12)? as usize,
        reservation_offset: read_u32(bytes, 16)? as usize,
        strings_size: read_u32(bytes, 32)? as usize,
        structure_size: read_u32(bytes, 36)? as usize,
    };
    if header.total_size < HEADER_SIZE || header.reservation_offset >= header.total_size {
        return Err(Error::Truncated);
    }
    if header.total_size > MAX_DTB_SIZE {
        return Err(Error::TooLarge);
    }
    checked_region(
        header.total_size,
        header.structure_offset,
        header.structure_size,
    )?;
    checked_region(
        header.total_size,
        header.strings_offset,
        header.strings_size,
    )?;
    Ok(header)
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32, Error> {
    let raw: [u8; 4] = bytes
        .get(offset..offset.checked_add(4).ok_or(Error::Truncated)?)
        .ok_or(Error::Truncated)?
        .try_into()
        .map_err(|_| Error::Truncated)?;
    Ok(u32::from_be_bytes(raw))
}

fn read_u64(bytes: &[u8], offset: usize) -> Result<u64, Error> {
    let raw: [u8; 8] = bytes
        .get(offset..offset.checked_add(8).ok_or(Error::Truncated)?)
        .ok_or(Error::Truncated)?
        .try_into()
        .map_err(|_| Error::Truncated)?;
    Ok(u64::from_be_bytes(raw))
}

fn take_u32(bytes: &[u8], cursor: &mut usize) -> Result<u32, Error> {
    let value = read_u32(bytes, *cursor)?;
    *cursor = cursor.checked_add(4).ok_or(Error::Truncated)?;
    Ok(value)
}

fn take_bytes<'a>(bytes: &'a [u8], cursor: &mut usize, length: usize) -> Result<&'a [u8], Error> {
    let end = cursor.checked_add(length).ok_or(Error::Truncated)?;
    let value = bytes.get(*cursor..end).ok_or(Error::Truncated)?;
    *cursor = end;
    Ok(value)
}

fn take_c_string<'a>(bytes: &'a [u8], cursor: &mut usize) -> Result<&'a str, Error> {
    let tail = bytes.get(*cursor..).ok_or(Error::Truncated)?;
    let length = tail
        .iter()
        .position(|byte| *byte == 0)
        .ok_or(Error::Truncated)?;
    let value = str::from_utf8(&tail[..length]).map_err(|_| Error::BadStructure)?;
    *cursor = cursor.checked_add(length + 1).ok_or(Error::Truncated)?;
    Ok(value)
}

fn take_string_at(strings: &[u8], offset: usize) -> Result<&str, Error> {
    let tail = strings.get(offset..).ok_or(Error::Truncated)?;
    let length = tail
        .iter()
        .position(|byte| *byte == 0)
        .ok_or(Error::Truncated)?;
    str::from_utf8(&tail[..length]).map_err(|_| Error::BadStructure)
}

fn align_cursor(cursor: &mut usize, limit: usize) -> Result<(), Error> {
    *cursor = cursor.checked_add(3).ok_or(Error::Truncated)? & !3;
    if *cursor > limit {
        return Err(Error::Truncated);
    }
    Ok(())
}

fn checked_region(total: usize, offset: usize, size: usize) -> Result<(), Error> {
    if offset
        .checked_add(size)
        .filter(|end| *end <= total)
        .is_none()
    {
        return Err(Error::Truncated);
    }
    Ok(())
}

fn region(bytes: &[u8], offset: usize, size: usize) -> Result<&[u8], Error> {
    let end = offset.checked_add(size).ok_or(Error::Truncated)?;
    bytes.get(offset..end).ok_or(Error::Truncated)
}
