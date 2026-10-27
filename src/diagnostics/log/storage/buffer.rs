// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Allocation-free variable-record log buffer.

use core::fmt;

const HEADER_SIZE: usize = 24;

/// Linux-compatible syslog severity values.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
#[repr(u8)]
pub enum Level {
    Emergency = 0,
    Alert = 1,
    Critical = 2,
    Error = 3,
    Warning = 4,
    Notice = 5,
    Info = 6,
    Debug = 7,
}

impl Level {
    pub const fn from_u8(value: u8) -> Option<Self> {
        match value {
            0 => Some(Self::Emergency),
            1 => Some(Self::Alert),
            2 => Some(Self::Critical),
            3 => Some(Self::Error),
            4 => Some(Self::Warning),
            5 => Some(Self::Notice),
            6 => Some(Self::Info),
            7 => Some(Self::Debug),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Timestamp(u64);

impl Timestamp {
    pub const fn from_microseconds(microseconds: u64) -> Self {
        Self(microseconds)
    }
}

impl fmt::Display for Timestamp {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let seconds = self.0 / 1_000_000;
        let microseconds = self.0 % 1_000_000;
        write!(formatter, "{seconds:>5}.{microseconds:06}")
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RecordFlags(u8);

impl RecordFlags {
    pub const NONE: Self = Self(0);
    pub const TRUNCATED: Self = Self(1 << 0);

    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AppendError {
    BufferTooSmall,
    Corrupt,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReadError {
    Corrupt,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Record {
    pub sequence: u64,
    pub timestamp_microseconds: u64,
    pub level: Level,
    pub flags: RecordFlags,
    pub length: usize,
    pub copied: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReadResult {
    Record(Record),
    Empty { next_sequence: u64 },
    Overrun { oldest_sequence: u64, missed: u64 },
}

/// Allocation-free variable-length record ring.
///
/// Records contain a fixed header followed by message bytes. Oldest complete
/// records are discarded when a new record needs space.
pub struct RingBuffer<const CAPACITY: usize> {
    bytes: [u8; CAPACITY],
    head: usize,
    tail: usize,
    used: usize,
    next_sequence: u64,
    dropped: u64,
}

impl<const CAPACITY: usize> RingBuffer<CAPACITY> {
    pub const fn new() -> Self {
        Self {
            bytes: [0; CAPACITY],
            head: 0,
            tail: 0,
            used: 0,
            next_sequence: 0,
            dropped: 0,
        }
    }

    pub const fn capacity(&self) -> usize {
        CAPACITY
    }

    pub const fn dropped(&self) -> u64 {
        self.dropped
    }

    pub const fn next_sequence(&self) -> u64 {
        self.next_sequence
    }

    pub fn append(
        &mut self,
        level: Level,
        timestamp_microseconds: u64,
        message: &[u8],
        mut flags: RecordFlags,
    ) -> Result<u64, AppendError> {
        if CAPACITY < HEADER_SIZE {
            return Err(AppendError::BufferTooSmall);
        }
        let maximum_payload = (CAPACITY - HEADER_SIZE).min(usize::from(u16::MAX));
        let payload_length = message.len().min(maximum_payload);
        if payload_length != message.len() {
            flags = flags.union(RecordFlags::TRUNCATED);
        }
        let total_length = HEADER_SIZE + payload_length;
        while CAPACITY - self.used < total_length {
            self.discard_oldest()?;
        }

        let sequence = self.next_sequence;
        self.next_sequence = self.next_sequence.wrapping_add(1);
        self.write_u32(self.head, total_length as u32);
        self.write_u64(self.offset(self.head, 4), sequence);
        self.write_u64(self.offset(self.head, 12), timestamp_microseconds);
        self.write_byte(self.offset(self.head, 20), level as u8);
        self.write_byte(self.offset(self.head, 21), flags.0);
        self.write_u16(self.offset(self.head, 22), payload_length as u16);
        self.write_slice(
            self.offset(self.head, HEADER_SIZE),
            &message[..payload_length],
        );
        self.head = self.offset(self.head, total_length);
        self.used += total_length;
        Ok(sequence)
    }

    pub fn read(&self, sequence: u64, output: &mut [u8]) -> Result<ReadResult, ReadError> {
        if self.used == 0 {
            return Ok(ReadResult::Empty {
                next_sequence: self.next_sequence,
            });
        }
        let oldest = self.read_u64(self.offset(self.tail, 4));
        if sequence < oldest {
            return Ok(ReadResult::Overrun {
                oldest_sequence: oldest,
                missed: oldest - sequence,
            });
        }

        let mut cursor = self.tail;
        let mut remaining = self.used;
        while remaining != 0 {
            let total_length = self.record_length(cursor).map_err(|_| ReadError::Corrupt)?;
            if total_length > remaining {
                return Err(ReadError::Corrupt);
            }
            let record_sequence = self.read_u64(self.offset(cursor, 4));
            if record_sequence >= sequence {
                let timestamp_microseconds = self.read_u64(self.offset(cursor, 12));
                let level = Level::from_u8(self.read_byte(self.offset(cursor, 20)))
                    .ok_or(ReadError::Corrupt)?;
                let flags = RecordFlags(self.read_byte(self.offset(cursor, 21)));
                let length = usize::from(self.read_u16(self.offset(cursor, 22)));
                if HEADER_SIZE + length != total_length {
                    return Err(ReadError::Corrupt);
                }
                let copied = length.min(output.len());
                self.read_slice(self.offset(cursor, HEADER_SIZE), &mut output[..copied]);
                return Ok(ReadResult::Record(Record {
                    sequence: record_sequence,
                    timestamp_microseconds,
                    level,
                    flags,
                    length,
                    copied,
                }));
            }
            cursor = self.offset(cursor, total_length);
            remaining -= total_length;
        }
        Ok(ReadResult::Empty {
            next_sequence: self.next_sequence,
        })
    }

    fn discard_oldest(&mut self) -> Result<(), AppendError> {
        if self.used == 0 {
            return Err(AppendError::Corrupt);
        }
        let length = self.record_length(self.tail)?;
        if length > self.used {
            return Err(AppendError::Corrupt);
        }
        self.tail = self.offset(self.tail, length);
        self.used -= length;
        self.dropped = self.dropped.wrapping_add(1);
        Ok(())
    }

    fn record_length(&self, offset: usize) -> Result<usize, AppendError> {
        let length = self.read_u32(offset) as usize;
        if !(HEADER_SIZE..=CAPACITY).contains(&length) {
            Err(AppendError::Corrupt)
        } else {
            Ok(length)
        }
    }

    fn offset(&self, base: usize, addition: usize) -> usize {
        (base + addition) % CAPACITY
    }

    fn write_byte(&mut self, offset: usize, value: u8) {
        self.bytes[offset] = value;
    }

    fn read_byte(&self, offset: usize) -> u8 {
        self.bytes[offset]
    }

    fn write_slice(&mut self, offset: usize, value: &[u8]) {
        for (index, byte) in value.iter().copied().enumerate() {
            self.write_byte(self.offset(offset, index), byte);
        }
    }

    fn read_slice(&self, offset: usize, output: &mut [u8]) {
        for (index, byte) in output.iter_mut().enumerate() {
            *byte = self.read_byte(self.offset(offset, index));
        }
    }

    fn write_u16(&mut self, offset: usize, value: u16) {
        self.write_slice(offset, &value.to_le_bytes());
    }

    fn read_u16(&self, offset: usize) -> u16 {
        let mut value = [0; 2];
        self.read_slice(offset, &mut value);
        u16::from_le_bytes(value)
    }

    fn write_u32(&mut self, offset: usize, value: u32) {
        self.write_slice(offset, &value.to_le_bytes());
    }

    fn read_u32(&self, offset: usize) -> u32 {
        let mut value = [0; 4];
        self.read_slice(offset, &mut value);
        u32::from_le_bytes(value)
    }

    fn write_u64(&mut self, offset: usize, value: u64) {
        self.write_slice(offset, &value.to_le_bytes());
    }

    fn read_u64(&self, offset: usize) -> u64 {
        let mut value = [0; 8];
        self.read_slice(offset, &mut value);
        u64::from_le_bytes(value)
    }
}

impl<const CAPACITY: usize> Default for RingBuffer<CAPACITY> {
    fn default() -> Self {
        Self::new()
    }
}
