// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Pure AMD SVM capability and exit decoding.

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SvmFeatures {
    pub revision: u32,
    pub asids: u32,
    pub nested_paging: bool,
    pub next_rip: bool,
    pub vmcb_clean: bool,
    pub flush_by_asid: bool,
    pub decode_assist: bool,
}

impl SvmFeatures {
    pub const fn decode(eax: u32, ebx: u32, edx: u32) -> Self {
        Self {
            revision: eax,
            asids: ebx,
            nested_paging: edx & 1 != 0,
            next_rip: edx & (1 << 3) != 0,
            vmcb_clean: edx & (1 << 5) != 0,
            flush_by_asid: edx & (1 << 6) != 0,
            decode_assist: edx & (1 << 7) != 0,
        }
    }

    pub const fn supports_backend(self) -> bool {
        self.asids > 1 && self.nested_paging && self.next_rip
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IoDirection {
    Input,
    Output,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IoExit {
    pub port: u16,
    pub size: usize,
    pub direction: IoDirection,
    pub string: bool,
    pub repeat: bool,
}

impl IoExit {
    pub const fn decode(exit_info: u64) -> Option<Self> {
        let size = match (exit_info >> 4) & 7 {
            1 => 1,
            2 => 2,
            4 => 4,
            _ => return None,
        };
        Some(Self {
            port: (exit_info >> 16) as u16,
            size,
            direction: if exit_info & 1 == 0 {
                IoDirection::Output
            } else {
                IoDirection::Input
            },
            string: exit_info & (1 << 2) != 0,
            repeat: exit_info & (1 << 3) != 0,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NptAccess {
    Read,
    Write,
    Execute,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NptViolation {
    pub access: NptAccess,
    pub during_page_walk: bool,
}

impl NptViolation {
    pub const fn decode(exit_info: u64) -> Self {
        let access = if exit_info & (1 << 1) != 0 {
            NptAccess::Write
        } else if exit_info & (1 << 4) != 0 {
            NptAccess::Execute
        } else {
            NptAccess::Read
        };
        Self {
            access,
            // Bit 33 reports a nested fault while hardware was reading a
            // guest paging-structure page; bit 32 describes the final GPA.
            during_page_walk: exit_info & (1 << 33) != 0,
        }
    }
}
