// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Firmware ISA claims used before any hart is admitted to the scheduler.

const I: u16 = 1 << 0;
const M: u16 = 1 << 1;
const A: u16 = 1 << 2;
const F: u16 = 1 << 3;
const D: u16 = 1 << 4;
const C: u16 = 1 << 5;
const ZICSR: u16 = 1 << 6;
const ZIFENCEI: u16 = 1 << 7;
const SSTC: u16 = 1 << 8;
const H: u16 = 1 << 9;
const ZICBOM: u16 = 1 << 10;
const BASELINE: u16 = I | M | A | F | D | C | ZICSR | ZIFENCEI | H;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Missing {
    Baseline,
    Timer,
    Cache,
}

/// Keep alternative bindings separate: the modern binding is authoritative,
/// never unioned with legacy claims which might contradict it.
#[derive(Clone, Copy)]
pub(super) struct Claims {
    legacy: Option<Result<u16, ()>>,
    modern: Option<Result<u16, ()>>,
    modern_base: Option<bool>,
}

impl Claims {
    pub(super) const EMPTY: Self = Self {
        legacy: None,
        modern: None,
        modern_base: None,
    };

    pub(super) fn property(&mut self, name: &str, bytes: &[u8]) {
        match name {
            "riscv,isa" => {
                self.legacy = Some(if self.legacy.is_some() {
                    Err(())
                } else {
                    legacy(bytes)
                })
            }
            "riscv,isa-base" => {
                self.modern_base = Some(self.modern_base.is_none() && bytes == b"rv64i\0")
            }
            "riscv,isa-extensions" => {
                self.modern = Some(if self.modern.is_some() {
                    Err(())
                } else {
                    modern(bytes)
                })
            }
            _ => {}
        }
    }

    pub(super) fn validate(self, enabled: bool) -> Result<(), Missing> {
        // Firmware can describe unavailable harts with incomplete ISA data.
        // They are excluded from both scheduler admission and the guarantee.
        if !enabled {
            return Ok(());
        }
        let bits = if let Some(modern) = self.modern {
            if self.modern_base != Some(true) {
                return Err(Missing::Baseline);
            }
            modern
        } else {
            // An incomplete modern binding must not silently fall back.
            if self.modern_base.is_some() {
                return Err(Missing::Baseline);
            }
            self.legacy.ok_or(Missing::Baseline)?
        }
        .map_err(|()| Missing::Baseline)?;
        if bits & BASELINE != BASELINE {
            return Err(Missing::Baseline);
        }
        if bits & SSTC == 0 {
            return Err(Missing::Timer);
        }
        if bits & ZICBOM == 0 {
            return Err(Missing::Cache);
        }
        Ok(())
    }
}

fn named(name: &[u8]) -> u16 {
    match name {
        b"i" => I,
        b"m" => M,
        b"a" => A,
        b"f" => F,
        b"d" => D,
        b"c" => C,
        b"h" => H,
        b"zicsr" => ZICSR,
        b"zifencei" => ZIFENCEI,
        b"sstc" => SSTC,
        b"zicbom" => ZICBOM,
        _ => 0,
    }
}

fn modern(bytes: &[u8]) -> Result<u16, ()> {
    let bytes = bytes.strip_suffix(&[0]).ok_or(())?;
    let mut bits = 0;
    for extension in bytes.split(|b| *b == 0) {
        if extension.is_empty()
            || !extension[0].is_ascii_lowercase()
            || !extension
                .iter()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        {
            return Err(());
        }
        let bit = named(extension);
        if bit != 0 && bits & bit != 0 {
            return Err(());
        }
        bits |= bit;
    }
    Ok(bits)
}

fn version(bytes: &[u8]) -> Result<usize, ()> {
    let mut end = 0;
    while end < bytes.len() && bytes[end].is_ascii_digit() {
        end += 1;
    }
    if end < bytes.len() && bytes[end] == b'p' {
        if end == 0 {
            return Err(());
        }
        end += 1;
        let start = end;
        while end < bytes.len() && bytes[end].is_ascii_digit() {
            end += 1;
        }
        if start == end {
            return Err(());
        }
    }
    Ok(end)
}

fn legacy(bytes: &[u8]) -> Result<u16, ()> {
    let bytes = bytes
        .strip_suffix(&[0])
        .ok_or(())?
        .strip_prefix(b"rv64")
        .ok_or(())?;
    let mut parts = bytes.split(|b| *b == b'_');
    let mut base = parts.next().ok_or(())?;
    if !matches!(base.first(), Some(b'i' | b'g')) {
        return Err(());
    }
    let mut bits = 0;
    while let Some((&letter, rest)) = base.split_first() {
        if !letter.is_ascii_lowercase() {
            return Err(());
        }
        bits |= if letter == b'g' {
            I | M | A | F | D | ZICSR | ZIFENCEI
        } else {
            named(&[letter])
        };
        base = &rest[version(rest)?..];
    }
    for extension in parts {
        if extension.is_empty()
            || !extension[0].is_ascii_lowercase()
            || !extension
                .iter()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        {
            return Err(());
        }
        // Digits can belong to an unknown extension name (zvl128b, zve32f),
        // so only interpret version suffixes after a recognized exact name.
        for name in [
            b"i".as_slice(),
            b"m",
            b"a",
            b"f",
            b"d",
            b"c",
            b"h",
            b"zicsr",
            b"zifencei",
            b"sstc",
            b"zicbom",
        ] {
            let Some(suffix) = extension.strip_prefix(name) else {
                continue;
            };
            if suffix.is_empty() {
                bits |= named(name);
                break;
            }
            if suffix[0].is_ascii_digit() {
                if version(suffix)? != suffix.len() {
                    return Err(());
                }
                bits |= named(name);
                break;
            }
        }
    }
    Ok(bits)
}

#[cfg(test)]
mod tests {
    use super::*;
    const GOOD: &[u8] = b"rv64imafdc_h_zicsr_zifencei_sstc_zicbom\0";
    const MODERN: &[u8] = b"i\0m\0a\0f\0d\0c\0h\0zicsr\0zifencei\0sstc\0zicbom\0";
    #[test]
    fn requires_complete_guaranteed_baseline() {
        let mut claims = Claims::EMPTY;
        claims.property("riscv,isa", GOOD);
        assert_eq!(claims.validate(true), Ok(()));
        for bad in [
            b"rv64imafdc_h_sstc_zicbom\0".as_slice(),
            b"rv32imafdc_h_zicsr_zifencei_sstc_zicbom\0",
            b"rv64imafdc_h_zicsr_zifencei_sstcfoo_zicbom\0",
            b"rv64imafdc_h_zicsr_zifencei_sstc2p_zicbom\0",
        ] {
            let mut claims = Claims::EMPTY;
            claims.property("riscv,isa", bad);
            assert!(claims.validate(true).is_err());
        }
    }
    #[test]
    fn modern_binding_is_authoritative_in_either_property_order() {
        for reverse in [false, true] {
            let mut claims = Claims::EMPTY;
            claims.property("riscv,isa-base", b"rv64i\0");
            let entries = [
                ("riscv,isa", GOOD),
                ("riscv,isa-extensions", b"i\0m\0a\0c\0".as_slice()),
            ];
            for i in 0..2 {
                let (name, bytes) = entries[if reverse { 1 - i } else { i }];
                claims.property(name, bytes);
            }
            assert_eq!(claims.validate(true), Err(Missing::Baseline));
        }
        let mut claims = Claims::EMPTY;
        claims.property("riscv,isa", b"malformed\0");
        claims.property("riscv,isa-extensions", MODERN);
        assert!(claims.validate(true).is_err());
        claims.property("riscv,isa-base", b"rv64i\0");
        assert_eq!(claims.validate(true), Ok(()));
    }
    #[test]
    fn disabled_cpu_claims_do_not_qualify_or_reject_enabled_harts() {
        let mut bad = Claims::EMPTY;
        bad.property("riscv,isa", b"invalid");
        assert_eq!(bad.validate(false), Ok(()));
        assert_eq!(bad.validate(true), Err(Missing::Baseline));
        let mut good = Claims::EMPTY;
        good.property("riscv,isa", GOOD);
        assert_eq!(good.validate(true), Ok(()));
        // A single good CPU cannot repair another enabled CPU's missing ISA.
        assert_eq!(Claims::EMPTY.validate(true), Err(Missing::Baseline));
    }
    #[test]
    fn unknown_digit_bearing_extension_names_are_not_versions_or_guarantees() {
        assert_eq!(
            legacy(b"rv64imafdc_h_zicsr_zifencei_sstc_zicbom_zvl128b_zve32f_zic64b\0"),
            legacy(GOOD)
        );
        assert_eq!(named(b"sstcfoo"), 0);
    }
    #[test]
    fn exact_versions_and_malformed_claims() {
        assert_eq!(
            legacy(b"rv64i2p1m2p0a2p1f2p2d2p2c2p0h1p0_zicsr2p0_zifencei2p0_sstc1p0_zicbom1p0\0"),
            legacy(GOOD)
        );
        assert!(legacy(b"rv64imafdc_h_zicsr_zifencei_sstc1p_zicbom\0").is_err());
        assert!(modern(b"i\0m").is_err());
        assert!(modern(b"i\0\0m\0").is_err());
        assert!(modern(b"i\0i\0").is_err());
    }
}
