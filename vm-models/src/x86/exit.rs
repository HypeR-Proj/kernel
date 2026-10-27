// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Owned policy events shared by the x86 VMX and SVM backends.

/// Width of an x86 port-I/O instruction supported by the initial device bus.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PortIoWidth {
    Byte,
    Word,
    DoubleWord,
}

impl PortIoWidth {
    pub const fn from_bytes(bytes: usize) -> Option<Self> {
        Some(match bytes {
            1 => Self::Byte,
            2 => Self::Word,
            4 => Self::DoubleWord,
            _ => return None,
        })
    }

    pub const fn bytes(self) -> usize {
        match self {
            Self::Byte => 1,
            Self::Word => 2,
            Self::DoubleWord => 4,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PortIoOperation {
    Input,
    Output(u32),
}

/// One decoded scalar x86 port-I/O operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PortIoExit {
    port: u16,
    width: PortIoWidth,
    operation: PortIoOperation,
}

impl PortIoExit {
    pub const fn new(port: u16, width: PortIoWidth, operation: PortIoOperation) -> Self {
        Self {
            port,
            width,
            operation,
        }
    }

    pub const fn port(self) -> u16 {
        self.port
    }

    pub const fn width(self) -> PortIoWidth {
        self.width
    }

    pub const fn operation(self) -> PortIoOperation {
        self.operation
    }
}

/// Policy completion for a scalar x86 port-I/O exit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[must_use]
pub enum PortIoAction {
    CompleteInput(u32),
    CompleteOutput,
    Stop,
}

/// Guest interrupt selected by x86 virtual interrupt-routing policy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[must_use]
pub enum PendingInterruptAction {
    None,
    Inject { vector: u8, consumes_timer: bool },
    Stop,
}
