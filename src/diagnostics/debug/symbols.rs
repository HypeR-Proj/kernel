// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Allocation-free lookup over an in-image ELF dynamic symbol table.

const ELF64_SYMBOL_SIZE: usize = 24;
const SHN_UNDEF: u16 = 0;
const STT_FUNC: u8 = 2;
pub const COMPACT_HEADER_SIZE: usize = 32;
pub const COMPACT_RECORD_SIZE: usize = 16;
pub const COMPACT_MAGIC: [u8; 8] = *b"HKALLSYM";
pub const COMPACT_VERSION: u32 = 1;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    AddressOverflow,
    InvalidStringTable,
    InvalidSymbolName,
    InvalidSymbolTable,
    InvalidCompactTable,
}

/// Lookup over the sorted, post-link symbol image embedded by the build tools.
pub struct CompactSymbolTable<'a> {
    records: &'a [u8],
    strings: &'a [u8],
    runtime_base: usize,
    image_size: usize,
}

impl<'a> CompactSymbolTable<'a> {
    pub fn new(bytes: &'a [u8], runtime_base: usize, image_size: usize) -> Result<Self, Error> {
        let header = bytes
            .get(..COMPACT_HEADER_SIZE)
            .ok_or(Error::InvalidCompactTable)?;
        if header.get(..8) != Some(COMPACT_MAGIC.as_slice())
            || read_u32(header, 8)? != COMPACT_VERSION
        {
            return Err(Error::InvalidCompactTable);
        }
        let count = read_u32(header, 12)? as usize;
        let strings_offset = read_u32(header, 16)? as usize;
        let strings_size = read_u32(header, 20)? as usize;
        let records_size = count
            .checked_mul(COMPACT_RECORD_SIZE)
            .ok_or(Error::InvalidCompactTable)?;
        let records_end = COMPACT_HEADER_SIZE
            .checked_add(records_size)
            .ok_or(Error::InvalidCompactTable)?;
        let strings_end = strings_offset
            .checked_add(strings_size)
            .ok_or(Error::InvalidCompactTable)?;
        if count == 0 || strings_offset != records_end || strings_end > bytes.len() {
            return Err(Error::InvalidCompactTable);
        }
        runtime_base
            .checked_add(image_size)
            .ok_or(Error::AddressOverflow)?;
        Ok(Self {
            records: &bytes[COMPACT_HEADER_SIZE..records_end],
            strings: &bytes[strings_offset..strings_end],
            runtime_base,
            image_size,
        })
    }

    pub fn lookup_containing(&self, address: usize) -> Result<Option<Symbol<'a>>, Error> {
        let Some(relative) = address.checked_sub(self.runtime_base) else {
            return Ok(None);
        };
        if relative >= self.image_size {
            return Ok(None);
        }
        let count = self.records.len() / COMPACT_RECORD_SIZE;
        let mut low = 0;
        let mut high = count;
        while low < high {
            let middle = low + (high - low) / 2;
            if self.record(middle)?.address <= relative as u64 {
                low = middle + 1;
            } else {
                high = middle;
            }
        }
        let Some(index) = low.checked_sub(1) else {
            return Ok(None);
        };
        let record = self.record(index)?;
        let start = usize::try_from(record.address).map_err(|_| Error::AddressOverflow)?;
        let end = start
            .checked_add(record.size as usize)
            .ok_or(Error::AddressOverflow)?;
        if relative < start || relative >= end {
            return Ok(None);
        }
        let name = symbol_name(self.strings, record.name_offset)?;
        Ok(Some(Symbol {
            name,
            address: self
                .runtime_base
                .checked_add(start)
                .ok_or(Error::AddressOverflow)?,
            size: record.size as usize,
            offset: relative - start,
        }))
    }

    fn record(&self, index: usize) -> Result<CompactRecord, Error> {
        let start = index
            .checked_mul(COMPACT_RECORD_SIZE)
            .ok_or(Error::InvalidCompactTable)?;
        let bytes = self
            .records
            .get(start..start + COMPACT_RECORD_SIZE)
            .ok_or(Error::InvalidCompactTable)?;
        Ok(CompactRecord {
            address: read_u64(bytes, 0)?,
            size: read_u32(bytes, 8)?,
            name_offset: read_u32(bytes, 12)?,
        })
    }
}

struct CompactRecord {
    address: u64,
    size: u32,
    name_offset: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Symbol<'a> {
    pub name: &'a str,
    pub address: usize,
    pub size: usize,
    pub offset: usize,
}

impl core::fmt::Display for Symbol<'_> {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match rustc_demangle::try_demangle(self.name) {
            Ok(name) => write!(formatter, "{name:#}+{:#x}/{:#x}", self.offset, self.size),
            Err(_) => write!(
                formatter,
                "{}+{:#x}/{:#x}",
                self.name, self.offset, self.size
            ),
        }
    }
}

/// A little-endian ELF64 symbol view with an explicit runtime load base.
pub struct SymbolTable<'a> {
    symbols: &'a [u8],
    strings: &'a [u8],
    runtime_base: usize,
    image_size: usize,
}

impl<'a> SymbolTable<'a> {
    pub fn new(
        symbols: &'a [u8],
        strings: &'a [u8],
        runtime_base: usize,
        image_size: usize,
    ) -> Result<Self, Error> {
        if symbols.is_empty() || !symbols.len().is_multiple_of(ELF64_SYMBOL_SIZE) {
            return Err(Error::InvalidSymbolTable);
        }
        if strings.first() != Some(&0) {
            return Err(Error::InvalidStringTable);
        }
        runtime_base
            .checked_add(image_size)
            .ok_or(Error::AddressOverflow)?;
        Ok(Self {
            symbols,
            strings,
            runtime_base,
            image_size,
        })
    }

    /// Resolves an address to the nearest preceding defined function symbol.
    pub fn lookup(&self, address: usize) -> Result<Option<Symbol<'a>>, Error> {
        self.lookup_matching(address, |_| true)
    }

    /// Resolves an address only when it lies inside a defined function.
    ///
    /// This stricter form is intended for diagnostics: a sparse dynamic symbol
    /// table must produce an unknown frame instead of attributing it to an
    /// unrelated earlier function.
    pub fn lookup_containing(&self, address: usize) -> Result<Option<Symbol<'a>>, Error> {
        self.lookup_matching(address, |symbol| {
            symbol.contains(address, self.runtime_base)
        })
    }

    fn lookup_matching(
        &self,
        address: usize,
        matches: impl Fn(ElfSymbol) -> bool,
    ) -> Result<Option<Symbol<'a>>, Error> {
        let Some(relative_address) = address.checked_sub(self.runtime_base) else {
            return Ok(None);
        };
        if relative_address >= self.image_size {
            return Ok(None);
        }

        let mut best: Option<ElfSymbol> = None;
        for bytes in self.symbols.chunks_exact(ELF64_SYMBOL_SIZE) {
            let symbol = ElfSymbol::parse(bytes)?;
            if !symbol.is_function()
                || symbol.value > relative_address as u64
                || symbol.value >= self.image_size as u64
                || !matches(symbol)
            {
                continue;
            }
            if best.is_none_or(|current| {
                symbol.value > current.value
                    || (symbol.value == current.value && symbol.size > current.size)
            }) {
                best = Some(symbol);
            }
        }

        let Some(best) = best else {
            return Ok(None);
        };
        let name = self.symbol_name(best.name_offset)?;
        let symbol_offset = usize::try_from(best.value).map_err(|_| Error::AddressOverflow)?;
        let symbol_address = self
            .runtime_base
            .checked_add(symbol_offset)
            .ok_or(Error::AddressOverflow)?;
        Ok(Some(Symbol {
            name,
            address: symbol_address,
            size: usize::try_from(best.size).map_err(|_| Error::AddressOverflow)?,
            offset: address - symbol_address,
        }))
    }

    fn symbol_name(&self, offset: u32) -> Result<&'a str, Error> {
        symbol_name(self.strings, offset)
    }
}

fn symbol_name(strings: &[u8], offset: u32) -> Result<&str, Error> {
    let bytes = strings
        .get(offset as usize..)
        .ok_or(Error::InvalidSymbolName)?;
    let length = bytes
        .iter()
        .position(|byte| *byte == 0)
        .ok_or(Error::InvalidSymbolName)?;
    core::str::from_utf8(&bytes[..length]).map_err(|_| Error::InvalidSymbolName)
}

#[derive(Clone, Copy)]
struct ElfSymbol {
    name_offset: u32,
    info: u8,
    section_index: u16,
    value: u64,
    size: u64,
}

impl ElfSymbol {
    fn parse(bytes: &[u8]) -> Result<Self, Error> {
        Ok(Self {
            name_offset: read_u32(bytes, 0)?,
            info: *bytes.get(4).ok_or(Error::InvalidSymbolTable)?,
            section_index: read_u16(bytes, 6)?,
            value: read_u64(bytes, 8)?,
            size: read_u64(bytes, 16)?,
        })
    }

    const fn is_function(self) -> bool {
        self.section_index != SHN_UNDEF && self.info & 0xf == STT_FUNC && self.name_offset != 0
    }

    fn contains(self, address: usize, runtime_base: usize) -> bool {
        let Ok(address) = u64::try_from(address.saturating_sub(runtime_base)) else {
            return false;
        };
        self.size != 0
            && self
                .value
                .checked_add(self.size)
                .is_some_and(|end| self.value <= address && address < end)
    }
}

fn read_u16(bytes: &[u8], offset: usize) -> Result<u16, Error> {
    let raw: [u8; 2] = bytes
        .get(offset..offset + 2)
        .ok_or(Error::InvalidSymbolTable)?
        .try_into()
        .map_err(|_| Error::InvalidSymbolTable)?;
    Ok(u16::from_le_bytes(raw))
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32, Error> {
    let raw: [u8; 4] = bytes
        .get(offset..offset + 4)
        .ok_or(Error::InvalidSymbolTable)?
        .try_into()
        .map_err(|_| Error::InvalidSymbolTable)?;
    Ok(u32::from_le_bytes(raw))
}

fn read_u64(bytes: &[u8], offset: usize) -> Result<u64, Error> {
    let raw: [u8; 8] = bytes
        .get(offset..offset + 8)
        .ok_or(Error::InvalidSymbolTable)?
        .try_into()
        .map_err(|_| Error::InvalidSymbolTable)?;
    Ok(u64::from_le_bytes(raw))
}
