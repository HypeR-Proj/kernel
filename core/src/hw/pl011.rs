// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! `PrimeCell` UART (PL011) programmer's model.
//!
//! Offsets and bit definitions follow Arm DDI 0183G. Keeping the complete
//! device register vocabulary here avoids scattering hardware literals across
//! physical drivers and virtual device models.

pub const DR: usize = 0x000;
pub const RSR_ECR: usize = 0x004;
pub const FR: usize = 0x018;
pub const ILPR: usize = 0x020;
pub const IBRD: usize = 0x024;
pub const FBRD: usize = 0x028;
pub const LCR_H: usize = 0x02c;
pub const CR: usize = 0x030;
pub const IFLS: usize = 0x034;
pub const IMSC: usize = 0x038;
pub const RIS: usize = 0x03c;
pub const MIS: usize = 0x040;
pub const ICR: usize = 0x044;
pub const DMACR: usize = 0x048;
pub const ITCR: usize = 0x080;
pub const ITIP: usize = 0x084;
pub const ITOP: usize = 0x08c;
pub const TDR: usize = 0x090;
pub const PERIPH_ID0: usize = 0xfe0;
pub const PERIPH_ID1: usize = 0xfe4;
pub const PERIPH_ID2: usize = 0xfe8;
pub const PERIPH_ID3: usize = 0xfec;
pub const PCELL_ID0: usize = 0xff0;
pub const PCELL_ID1: usize = 0xff4;
pub const PCELL_ID2: usize = 0xff8;
pub const PCELL_ID3: usize = 0xffc;

pub const DR_DATA_MASK: u32 = 0xff;
pub const DR_FE: u32 = 1 << 8;
pub const DR_PE: u32 = 1 << 9;
pub const DR_BE: u32 = 1 << 10;
pub const DR_OE: u32 = 1 << 11;
pub const DR_ERROR_MASK: u32 = DR_FE | DR_PE | DR_BE | DR_OE;

pub const RSR_FE: u32 = 1 << 0;
pub const RSR_PE: u32 = 1 << 1;
pub const RSR_BE: u32 = 1 << 2;
pub const RSR_OE: u32 = 1 << 3;
pub const RSR_ERROR_MASK: u32 = RSR_FE | RSR_PE | RSR_BE | RSR_OE;

pub const FR_CTS: u32 = 1 << 0;
pub const FR_DSR: u32 = 1 << 1;
pub const FR_DCD: u32 = 1 << 2;
pub const FR_BUSY: u32 = 1 << 3;
pub const FR_RXFE: u32 = 1 << 4;
pub const FR_TXFF: u32 = 1 << 5;
pub const FR_RXFF: u32 = 1 << 6;
pub const FR_TXFE: u32 = 1 << 7;
pub const FR_RI: u32 = 1 << 8;

pub const LCR_H_BRK: u32 = 1 << 0;
pub const LCR_H_PEN: u32 = 1 << 1;
pub const LCR_H_EPS: u32 = 1 << 2;
pub const LCR_H_STP2: u32 = 1 << 3;
pub const LCR_H_FEN: u32 = 1 << 4;
pub const LCR_H_WLEN_SHIFT: u32 = 5;
pub const LCR_H_WLEN_MASK: u32 = 0x3 << LCR_H_WLEN_SHIFT;
pub const LCR_H_SPS: u32 = 1 << 7;

pub const CR_UARTEN: u32 = 1 << 0;
pub const CR_SIREN: u32 = 1 << 1;
pub const CR_SIRLP: u32 = 1 << 2;
pub const CR_LBE: u32 = 1 << 7;
pub const CR_TXE: u32 = 1 << 8;
pub const CR_RXE: u32 = 1 << 9;
pub const CR_DTR: u32 = 1 << 10;
pub const CR_RTS: u32 = 1 << 11;
pub const CR_OUT1: u32 = 1 << 12;
pub const CR_OUT2: u32 = 1 << 13;
pub const CR_RTSEN: u32 = 1 << 14;
pub const CR_CTSEN: u32 = 1 << 15;
pub const CR_MASK: u32 = 0xff87;

pub const IFLS_TX_SHIFT: u32 = 0;
pub const IFLS_RX_SHIFT: u32 = 3;
pub const IFLS_FIELD_MASK: u32 = 0x7;
pub const IFLS_MASK: u32 = 0x3f;

pub const INT_RI: u32 = 1 << 0;
pub const INT_CTS: u32 = 1 << 1;
pub const INT_DCD: u32 = 1 << 2;
pub const INT_DSR: u32 = 1 << 3;
pub const INT_RX: u32 = 1 << 4;
pub const INT_TX: u32 = 1 << 5;
pub const INT_RT: u32 = 1 << 6;
pub const INT_FE: u32 = 1 << 7;
pub const INT_PE: u32 = 1 << 8;
pub const INT_BE: u32 = 1 << 9;
pub const INT_OE: u32 = 1 << 10;
pub const INT_MODEM_MASK: u32 = INT_RI | INT_CTS | INT_DCD | INT_DSR;
pub const INT_RECEIVE_MASK: u32 = INT_RX | INT_RT;
pub const INT_ERROR_MASK: u32 = INT_FE | INT_PE | INT_BE | INT_OE;
pub const INT_ALL: u32 = 0x7ff;

pub const DMACR_RXDMAE: u32 = 1 << 0;
pub const DMACR_TXDMAE: u32 = 1 << 1;
pub const DMACR_DMAONERR: u32 = 1 << 2;
pub const DMACR_MASK: u32 = 0x7;

pub const ITCR_ITEN: u32 = 1 << 0;

pub const PERIPH_ID0_VALUE: u32 = 0x11;
pub const PERIPH_ID1_VALUE: u32 = 0x10;
pub const PERIPH_ID2_VALUE: u32 = 0x34;
pub const PERIPH_ID3_VALUE: u32 = 0x00;
pub const PCELL_ID0_VALUE: u32 = 0x0d;
pub const PCELL_ID1_VALUE: u32 = 0xf0;
pub const PCELL_ID2_VALUE: u32 = 0x05;
pub const PCELL_ID3_VALUE: u32 = 0xb1;
