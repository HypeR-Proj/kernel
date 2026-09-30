// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

use crate::aarch64_exposed_copy::copy;

#[test]
fn exposed_copy_preserves_heads_tails_and_all_alignment_combinations() {
    let source: Vec<u8> = (0..4200).map(|i| (i % 251) as u8).collect();
    let mut destination = vec![0; source.len()];
    for source_offset in 0..16 {
        for destination_offset in 0..16 {
            for length in (0..=65).chain([127, 128, 129, 511, 512, 513, 4095, 4096, 4097]) {
                destination.fill(0xa5);
                // SAFETY: Separate live allocations provide initialized Normal
                // memory for every tested range. Neither range can fault or
                // overlap, and no pointer escapes this synchronous copy.
                unsafe {
                    copy(
                        source.as_ptr().add(source_offset),
                        destination.as_mut_ptr().add(destination_offset),
                        length,
                    );
                }
                assert_eq!(
                    &destination[destination_offset..destination_offset + length],
                    &source[source_offset..source_offset + length],
                    "source={source_offset}, destination={destination_offset}, length={length}"
                );
                assert!(destination[..destination_offset].iter().all(|&v| v == 0xa5));
                assert!(
                    destination[destination_offset + length..]
                        .iter()
                        .all(|&v| v == 0xa5)
                );
            }
        }
    }
}

#[cfg(target_os = "macos")]
mod guarded {
    use super::copy;
    use core::ffi::{c_int, c_void};

    unsafe extern "C" {
        fn getpagesize() -> c_int;
        fn mmap(
            address: *mut c_void,
            length: usize,
            protection: c_int,
            flags: c_int,
            fd: c_int,
            offset: i64,
        ) -> *mut c_void;
        fn mprotect(address: *mut c_void, length: usize, protection: c_int) -> c_int;
        fn munmap(address: *mut c_void, length: usize) -> c_int;
    }

    struct GuardedPage {
        allocation: *mut c_void,
        bytes: *mut u8,
        length: usize,
    }

    impl GuardedPage {
        fn new() -> Self {
            // SAFETY: getpagesize has no pointer preconditions. Darwin mmap
            // constants are MAP_PRIVATE | MAP_ANON, with PROT_NONE initially.
            let length = unsafe { getpagesize() } as usize;
            assert!(length >= 4096);
            // SAFETY: A null placement hint, anonymous fd=-1 and zero offset
            // request a fresh allocation; no existing mapping is replaced.
            let allocation = unsafe { mmap(core::ptr::null_mut(), length * 3, 0, 0x1002, -1, 0) };
            assert_ne!(allocation as usize, usize::MAX);
            // SAFETY: mmap returned three complete resident virtual pages.
            let bytes = unsafe { allocation.cast::<u8>().add(length) };
            let page = Self {
                allocation,
                bytes,
                length,
            };
            // SAFETY: Enable read/write only for the middle page we own.
            assert_eq!(unsafe { mprotect(bytes.cast(), length, 3) }, 0);
            page
        }
    }

    impl Drop for GuardedPage {
        fn drop(&mut self) {
            // SAFETY: The mapping is exclusively owned and no copy or borrow
            // remains live. The original base and complete length are retained.
            unsafe { munmap(self.allocation, self.length * 3) };
        }
    }

    #[test]
    fn exposed_copy_never_accesses_adjacent_guard_pages() {
        let source = GuardedPage::new();
        let destination = GuardedPage::new();
        // SAFETY: These separate middle pages are mapped read/write for this
        // test's lifetime. The assembly reads source and writes destination
        // synchronously, never retaining a pointer or crossing either guard.
        unsafe {
            let input = core::slice::from_raw_parts_mut(source.bytes, source.length);
            for (index, byte) in input.iter_mut().enumerate() {
                *byte = (index % 251) as u8;
            }
            core::ptr::write_bytes(destination.bytes, 0, destination.length);
            for length in
                (0..=65).chain([127, 128, 129, 4095, 4096, source.length - 1, source.length])
            {
                for at_end in [false, true] {
                    let offset = if at_end { source.length - length } else { 0 };
                    copy(
                        input.as_ptr().add(offset),
                        destination.bytes.add(offset),
                        length,
                    );
                    assert_eq!(
                        core::slice::from_raw_parts(destination.bytes.add(offset), length),
                        &input[offset..offset + length]
                    );
                }
            }
        }
    }
}
