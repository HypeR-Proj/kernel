// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Native affinity service calls under borrowed Process authority.

use super::ProcessServices;
use crate::mm::user_space::UserSlice;
use crate::process::ProcessError;
use crate::task::scheduler::CpuMask;

impl ProcessServices<'_> {
    pub(super) fn copy_affinity(
        &self,
        input: Option<UserSlice>,
        word_count: usize,
    ) -> Result<CpuMask, AffinityInputError> {
        const WORD_BYTES: usize = core::mem::size_of::<u64>();
        const MAX_WORDS: usize =
            hyper::abi::native::HYPER_NATIVE_PROCESS_AFFINITY_MAX_WORDS as usize;

        if word_count > MAX_WORDS {
            return Err(AffinityInputError::Invalid);
        }
        let byte_count = word_count
            .checked_mul(WORD_BYTES)
            .ok_or(AffinityInputError::Invalid)?;
        let mut encoded = [0_u8; MAX_WORDS * WORD_BYTES];
        if let Some(input) = input {
            if input.length() != byte_count as u64 {
                return Err(AffinityInputError::Invalid);
            }
            self.process
                .copy_from_user(input, &mut encoded[..byte_count])?;
        } else if byte_count != 0 {
            return Err(AffinityInputError::Invalid);
        }

        let mut words = [0_u64; MAX_WORDS];
        for (index, destination) in words[..word_count].iter_mut().enumerate() {
            let offset = index * WORD_BYTES;
            let mut word = [0_u8; WORD_BYTES];
            word.copy_from_slice(&encoded[offset..offset + WORD_BYTES]);
            *destination = u64::from_le_bytes(word);
        }
        for cpu in hyper::cpu::MAX_CPUS..word_count * u64::BITS as usize {
            if words[cpu / u64::BITS as usize] & (1_u64 << (cpu % u64::BITS as usize)) != 0 {
                return Err(AffinityInputError::Invalid);
            }
        }
        let mut affinity = CpuMask::EMPTY;
        for cpu in 0..hyper::cpu::MAX_CPUS {
            if words[cpu / u64::BITS as usize] & (1_u64 << (cpu % u64::BITS as usize)) == 0 {
                continue;
            }
            let Some(cpu) = hyper::cpu::CpuIndex::new(cpu) else {
                return Err(AffinityInputError::Invalid);
            };
            affinity = affinity.with_cpu(cpu);
        }
        Ok(affinity)
    }
}

pub(super) enum AffinityInputError {
    Invalid,
    Memory(ProcessError),
}

impl From<ProcessError> for AffinityInputError {
    fn from(error: ProcessError) -> Self {
        Self::Memory(error)
    }
}
