// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Checked byte copies across a managed foreign address-space boundary.

/// A page-oriented address space whose mappings are owned by another
/// protection domain.
///
/// Implementations retain responsibility for mapping lifetime and
/// synchronization. The copy helpers validate the complete range before the
/// backend sees any access and never pass a slice that crosses a page.
pub trait ForeignMemory {
    type Error;

    fn address_base(&self) -> u64;
    fn address_size(&self) -> u64;
    fn page_size(&self) -> usize;

    fn read_page(
        &mut self,
        page_index: usize,
        page_offset: usize,
        destination: &mut [u8],
    ) -> Result<(), Self::Error>;

    fn write_page(
        &mut self,
        page_index: usize,
        page_offset: usize,
        source: &[u8],
    ) -> Result<(), Self::Error>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ForeignCopyError<BackendError> {
    AddressOverflow,
    Backend(BackendError),
    InvalidPageSize,
    InvalidRange,
}

/// Copies bytes from a managed foreign address space into kernel-owned memory.
pub fn copy_from_foreign<Memory: ForeignMemory + ?Sized>(
    memory: &mut Memory,
    source_address: u64,
    destination: &mut [u8],
) -> Result<(), ForeignCopyError<Memory::Error>> {
    let (mut offset, page_size) = validate(memory, source_address, destination.len())?;
    let mut copied = 0;
    while copied < destination.len() {
        let page_index =
            usize::try_from(offset / page_size).map_err(|_| ForeignCopyError::AddressOverflow)?;
        let page_offset =
            usize::try_from(offset % page_size).map_err(|_| ForeignCopyError::AddressOverflow)?;
        let remaining_in_page = usize::try_from(page_size)
            .map_err(|_| ForeignCopyError::InvalidPageSize)?
            - page_offset;
        let length = remaining_in_page.min(destination.len() - copied);
        memory
            .read_page(
                page_index,
                page_offset,
                &mut destination[copied..copied + length],
            )
            .map_err(ForeignCopyError::Backend)?;
        copied += length;
        offset += length as u64;
    }
    Ok(())
}

/// Copies bytes from kernel-owned memory into a managed foreign address space.
pub fn copy_to_foreign<Memory: ForeignMemory + ?Sized>(
    memory: &mut Memory,
    destination_address: u64,
    source: &[u8],
) -> Result<(), ForeignCopyError<Memory::Error>> {
    let (mut offset, page_size) = validate(memory, destination_address, source.len())?;
    let mut copied = 0;
    while copied < source.len() {
        let page_index =
            usize::try_from(offset / page_size).map_err(|_| ForeignCopyError::AddressOverflow)?;
        let page_offset =
            usize::try_from(offset % page_size).map_err(|_| ForeignCopyError::AddressOverflow)?;
        let remaining_in_page = usize::try_from(page_size)
            .map_err(|_| ForeignCopyError::InvalidPageSize)?
            - page_offset;
        let length = remaining_in_page.min(source.len() - copied);
        memory
            .write_page(page_index, page_offset, &source[copied..copied + length])
            .map_err(ForeignCopyError::Backend)?;
        copied += length;
        offset += length as u64;
    }
    Ok(())
}

fn validate<Memory: ForeignMemory + ?Sized>(
    memory: &Memory,
    address: u64,
    length: usize,
) -> Result<(u64, u64), ForeignCopyError<Memory::Error>> {
    let page_size = u64::try_from(memory.page_size())
        .ok()
        .filter(|size| *size != 0)
        .ok_or(ForeignCopyError::InvalidPageSize)?;
    let offset = address
        .checked_sub(memory.address_base())
        .ok_or(ForeignCopyError::InvalidRange)?;
    let length = u64::try_from(length).map_err(|_| ForeignCopyError::AddressOverflow)?;
    let end = offset
        .checked_add(length)
        .ok_or(ForeignCopyError::AddressOverflow)?;
    if end > memory.address_size() {
        return Err(ForeignCopyError::InvalidRange);
    }
    Ok((offset, page_size))
}
