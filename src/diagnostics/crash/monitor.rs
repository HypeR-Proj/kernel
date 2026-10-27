// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

//! Allocation-free interactive diagnostics after fatal crash coordination.
//!
//! The monitor reads snapshots already published by the crash owner and never
//! resumes CPUs, changes ownership, or writes crash tables. It runs only after
//! emergency logging is active; commands must remain bounded or explicitly
//! polling and must not allocate or acquire ordinary kernel locks.

use core::fmt::{self, Write};
use core::hint::spin_loop;
use core::ptr::read_volatile;

use hyper::hal::memory::{KernelImageLayout, Stage1Mapping, Stage1MemoryType, VirtualMemoryLayout};
use hyper::sync::PublishedOnce;
use {hyper::mm::ranges::PhysicalRange, hyper_platform::PlatformInfo};

use super::report::StopSummary;
use super::state::{self, MAX_CPUS};

const LINE_CAPACITY: usize = 128;
const DEFAULT_DUMP_BYTES: usize = 64;
const MAX_DUMP_BYTES: usize = 256;

#[derive(Clone, Copy)]
struct MemorySnapshot {
    platform: PlatformInfo,
    virtual_layout: VirtualMemoryLayout,
    image: KernelImageLayout,
    root: u64,
    kernel_base: u64,
}

static MEMORY_SNAPSHOT: PublishedOnce<MemorySnapshot> = PublishedOnce::new();

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum InitializationError {
    AlreadyInitialized,
}

pub(super) fn initialize() -> Result<(), InitializationError> {
    let image = crate::boot::image::layout();
    let snapshot = crate::boot::with_boot_state(|state| MemorySnapshot {
        platform: state.platform,
        virtual_layout: crate::mm::memory::virtual_memory_layout(),
        image: KernelImageLayout {
            physical_start: state.image_physical_start,
            text_size: image.text_size,
            rodata_size: image.rodata_size,
            total_size: image.total_size,
        },
        root: state.memory.root_address(),
        kernel_base: state.memory.kernel_base(),
    });
    MEMORY_SNAPSHOT
        .publish(snapshot)
        .map_err(|_| InitializationError::AlreadyInitialized)
}

pub(super) fn run(owner: usize, stop: StopSummary) {
    write_raw(b"\nHypeR crash console\n");
    write_raw(b"The kernel is stopped. Type 'help' for commands.\n");
    let mut line = [0u8; LINE_CAPACITY];
    let mut ignore_line_feed = false;
    loop {
        write_raw(b"crash> ");
        let length = read_line(&mut line, &mut ignore_line_feed);
        let Ok(command) = core::str::from_utf8(&line[..length]) else {
            write_raw(b"input is not valid UTF-8\n");
            continue;
        };
        if execute(command, owner, stop) == Action::Halt {
            return;
        }
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum Action {
    Continue,
    Halt,
}

fn execute(line: &str, owner: usize, stop: StopSummary) -> Action {
    let mut arguments = line.split_ascii_whitespace();
    let Some(command) = arguments.next() else {
        return Action::Continue;
    };
    match command {
        "help" | "?" => print_help(),
        "status" => print_status(owner, stop),
        "cpus" => print_cpus(owner),
        "regs" => with_cpu_argument(&mut arguments, owner, dump_cpu_registers),
        "bt" => with_cpu_argument(&mut arguments, owner, dump_cpu_backtrace),
        "mappings" => print_mappings(),
        "map" => inspect_mapping_argument(&mut arguments),
        "x" => dump_memory_arguments(&mut arguments),
        "selftest" => run_self_test(owner, stop),
        "halt" => return Action::Halt,
        _ => write_line(format_args!("unknown command '{command}'; type 'help'")),
    }
    Action::Continue
}

fn print_help() {
    write_raw(
        b"commands:\n\
          help                 show this command list\n\
          status               show crash owner and stopped CPU state\n\
          cpus                 list captured CPU contexts\n\
          regs [cpu]           dump captured registers\n\
          bt [cpu]             dump a captured call trace\n\
          mappings             show configured kernel, RAM, and MMIO mappings\n\
          map <va>             inspect the live stage-1 leaf for a VA\n\
          x <va> [bytes]       dump up to 256 bytes from mapped RAM\n\
          selftest             validate crash-console invariants\n\
          halt                 leave the console and halt permanently\n",
    );
}

fn print_status(owner: usize, stop: StopSummary) {
    write_line(format_args!("crash owner: CPU {owner}"));
    write_line(format_args!(
        "other CPUs: stopped {}/{}, crash-stop IPI {}",
        stop.stopped,
        stop.expected,
        if stop.sent { "sent" } else { "not sent" }
    ));
    write_line(format_args!(
        "memory snapshot: {}",
        if MEMORY_SNAPSHOT.get().is_some() {
            "available"
        } else {
            "unavailable"
        }
    ));
}

fn print_cpus(owner: usize) {
    let online = crate::cpu::online_cpu_count().max(owner.saturating_add(1));
    for (cpu, slot) in state::contexts()
        .iter()
        .enumerate()
        .take(online.min(MAX_CPUS))
    {
        match slot.read() {
            Some(context) => write_line(format_args!(
                "CPU {cpu}: {} PC={:#018x} SP={:#018x}",
                if cpu == owner { "crashing" } else { "stopped" },
                context.program_counter,
                context.stack_pointer
            )),
            None => write_line(format_args!("CPU {cpu}: context unavailable")),
        }
    }
}

fn with_cpu_argument<'a>(
    arguments: &mut impl Iterator<Item = &'a str>,
    owner: usize,
    operation: fn(usize),
) {
    let cpu = match arguments.next() {
        Some(value) => match parse_usize(value) {
            Some(cpu) => cpu,
            None => {
                write_raw(b"invalid CPU index\n");
                return;
            }
        },
        None => owner,
    };
    if arguments.next().is_some() {
        write_raw(b"too many arguments\n");
        return;
    }
    operation(cpu);
}

fn dump_cpu_registers(cpu: usize) {
    let Some(context) = state::context(cpu) else {
        write_line(format_args!("CPU {cpu} context unavailable"));
        return;
    };
    super::report::dump_registers(&context);
}

fn dump_cpu_backtrace(cpu: usize) {
    let Some(context) = state::context(cpu) else {
        write_line(format_args!("CPU {cpu} context unavailable"));
        return;
    };
    let task = crate::task::scheduler::crash_snapshot(cpu);
    super::unwind::dump_backtrace(cpu, &context, task);
}

fn print_mappings() {
    let Some(snapshot) = MEMORY_SNAPSHOT.get().copied() else {
        write_raw(b"memory snapshot unavailable\n");
        return;
    };
    write_line(format_args!("stage-1 root PA {:#018x}", snapshot.root));
    print_kernel_segments(snapshot);
    for range in snapshot.platform.memory.as_slice() {
        let Some(virtual_start) = snapshot
            .virtual_layout
            .linear_base
            .checked_add(range.start())
        else {
            write_raw(b"linear RAM address overflow\n");
            continue;
        };
        write_line(format_args!(
            "linear RAM  VA {virtual_start:#018x}-{:#018x} -> PA {:#018x}-{:#018x} RW NX",
            virtual_start.saturating_add(range.size()),
            range.start(),
            range.end()
        ));
    }
    for range in snapshot.platform.no_map.as_slice() {
        write_line(format_args!(
            "no-map      PA {:#018x}-{:#018x}",
            range.start(),
            range.end()
        ));
    }
    for range in snapshot.platform.mmio.as_slice() {
        let Some(virtual_start) = snapshot.virtual_layout.mmio_base.checked_add(range.start())
        else {
            write_raw(b"MMIO address overflow\n");
            continue;
        };
        write_line(format_args!(
            "device      VA {virtual_start:#018x}-{:#018x} -> PA {:#018x}-{:#018x} RW NX",
            virtual_start.saturating_add(range.size()),
            range.start(),
            range.end()
        ));
    }
}

fn print_kernel_segments(snapshot: MemorySnapshot) {
    let text_end = snapshot
        .kernel_base
        .saturating_add(snapshot.image.text_size);
    let rodata_end = text_end.saturating_add(snapshot.image.rodata_size);
    let image_end = snapshot
        .kernel_base
        .saturating_add(snapshot.image.total_size);
    write_line(format_args!(
        "kernel text VA {:#018x}-{text_end:#018x} -> PA {:#018x} RO X",
        snapshot.kernel_base, snapshot.image.physical_start
    ));
    write_line(format_args!(
        "kernel ro   VA {text_end:#018x}-{rodata_end:#018x} -> PA {:#018x} RO NX",
        snapshot
            .image
            .physical_start
            .saturating_add(snapshot.image.text_size)
    ));
    write_line(format_args!(
        "kernel data VA {rodata_end:#018x}-{image_end:#018x} -> PA {:#018x} RW NX",
        snapshot
            .image
            .physical_start
            .saturating_add(snapshot.image.text_size)
            .saturating_add(snapshot.image.rodata_size)
    ));
}

fn inspect_mapping_argument<'a>(arguments: &mut impl Iterator<Item = &'a str>) {
    let Some(address) = parse_required_address(arguments) else {
        return;
    };
    if arguments.next().is_some() {
        write_raw(b"usage: map <virtual-address>\n");
        return;
    }
    let Some(snapshot) = MEMORY_SNAPSHOT.get().copied() else {
        write_raw(b"memory snapshot unavailable\n");
        return;
    };
    match inspect_mapping(snapshot, address) {
        Ok(Some(mapping)) => print_mapping(address, mapping),
        Ok(None) => write_line(format_args!("{address:#018x}: unmapped")),
        Err(error) => write_line(format_args!("page-table walk failed: {error:?}")),
    }
}

fn print_mapping(address: usize, mapping: Stage1Mapping) {
    let offset = (address as u64).saturating_sub(mapping.virtual_start);
    let physical = mapping.physical_start.saturating_add(offset);
    let memory_type = match mapping.memory_type {
        Stage1MemoryType::Normal => "normal",
        Stage1MemoryType::Device => "device",
        Stage1MemoryType::Unknown => "unknown",
    };
    let access = match (mapping.readable, mapping.writable) {
        (true, true) => "RW",
        (true, false) => "RO",
        (false, true) => "WO",
        (false, false) => "--",
    };
    write_line(format_args!(
        "VA {address:#018x} -> PA {physical:#018x}; leaf {:#018x}-{:#018x}, {access} {} {}, {memory_type}",
        mapping.virtual_start,
        mapping.virtual_start.saturating_add(mapping.size),
        if mapping.executable { "X" } else { "NX" },
        mapping.size
    ));
}

fn dump_memory_arguments<'a>(arguments: &mut impl Iterator<Item = &'a str>) {
    let Some(address) = parse_required_address(arguments) else {
        return;
    };
    let length = match arguments.next() {
        Some(value) => match parse_usize(value) {
            Some(length) if (1..=MAX_DUMP_BYTES).contains(&length) => length,
            _ => {
                write_line(format_args!(
                    "byte count must be between 1 and {MAX_DUMP_BYTES}"
                ));
                return;
            }
        },
        None => DEFAULT_DUMP_BYTES,
    };
    if arguments.next().is_some() {
        write_raw(b"usage: x <virtual-address> [byte-count]\n");
        return;
    }
    let Some(snapshot) = MEMORY_SNAPSHOT.get().copied() else {
        write_raw(b"memory snapshot unavailable\n");
        return;
    };
    if let Err(error) = validate_read(snapshot, address, length) {
        write_line(format_args!("memory read rejected: {error}"));
        return;
    }
    for row in (0..length).step_by(16) {
        write_line_start(format_args!("{:#018x}:", address.saturating_add(row)));
        for offset in row..(row + 16).min(length) {
            // SAFETY: validate_read walked every covering leaf and admitted
            // only readable mappings backed by non-no-map platform RAM.
            let byte = unsafe { read_volatile(address.saturating_add(offset) as *const u8) };
            write_fragment(format_args!(" {byte:02x}"));
        }
        write_raw(b"\n");
    }
}

fn validate_read(
    snapshot: MemorySnapshot,
    address: usize,
    length: usize,
) -> Result<(), &'static str> {
    let end = address.checked_add(length).ok_or("address overflow")?;
    let mut cursor = address;
    while cursor < end {
        let mapping = inspect_mapping(snapshot, cursor)
            .map_err(|_| "page-table walk failed")?
            .ok_or("unmapped address")?;
        if !mapping.readable {
            return Err("mapping is not readable");
        }
        if mapping.memory_type == Stage1MemoryType::Device {
            return Err("device mappings cannot be inspected");
        }
        let offset = (cursor as u64)
            .checked_sub(mapping.virtual_start)
            .ok_or("invalid mapping extent")?;
        let physical = mapping
            .physical_start
            .checked_add(offset)
            .ok_or("physical address overflow")?;
        let available = usize::try_from(mapping.size.saturating_sub(offset))
            .map_err(|_| "mapping extent is not representable")?;
        let chunk = available.min(end - cursor);
        if chunk == 0 || !physical_range_is_safe(snapshot, physical, chunk) {
            return Err("mapping is not backed by inspectable RAM");
        }
        cursor = cursor.checked_add(chunk).ok_or("address overflow")?;
    }
    Ok(())
}

fn physical_range_is_safe(snapshot: MemorySnapshot, start: u64, length: usize) -> bool {
    let Some(range) = PhysicalRange::new(start, length as u64) else {
        return false;
    };
    snapshot
        .platform
        .memory
        .as_slice()
        .iter()
        .any(|memory| memory.start() <= range.start() && range.end() <= memory.end())
        && !snapshot
            .platform
            .no_map
            .as_slice()
            .iter()
            .any(|excluded| excluded.overlaps(range))
}

fn run_self_test(owner: usize, stop: StopSummary) {
    let mut passed = 0usize;
    let mut failed = 0usize;
    report_test(
        "emergency console",
        crate::diagnostics::log::crash_console_available(),
        &mut passed,
        &mut failed,
    );
    let snapshot = MEMORY_SNAPSHOT.get().copied();
    report_test(
        "memory snapshot",
        snapshot.is_some(),
        &mut passed,
        &mut failed,
    );
    let owner_context = state::context(owner);
    report_test(
        "owner crash context",
        owner_context.is_some(),
        &mut passed,
        &mut failed,
    );
    report_test(
        "remote CPU stop",
        stop.expected == stop.stopped,
        &mut passed,
        &mut failed,
    );
    let owner_pc_is_mapped = match (snapshot, owner_context) {
        (Some(snapshot), Some(context)) => usize::try_from(context.program_counter)
            .ok()
            .and_then(|address| inspect_mapping(snapshot, address).ok())
            .flatten()
            .is_some(),
        _ => false,
    };
    report_test(
        "owner PC stage-1 mapping",
        owner_pc_is_mapped,
        &mut passed,
        &mut failed,
    );
    write_line(format_args!("selftest: {passed} passed, {failed} failed"));
}

fn inspect_mapping(
    snapshot: MemorySnapshot,
    address: usize,
) -> Result<Option<Stage1Mapping>, crate::hal::memory::Error> {
    // SAFETY: MemorySnapshot is published from the permanently installed boot
    // address space. Its root and all page-table pages remain pinned and
    // linearly accessible throughout crash-console execution.
    unsafe { crate::hal::memory::inspect_stage1_mapping(snapshot.root, address) }
}

fn report_test(name: &str, result: bool, passed: &mut usize, failed: &mut usize) {
    if result {
        *passed += 1;
        write_line(format_args!("PASS: {name}"));
    } else {
        *failed += 1;
        write_line(format_args!("FAIL: {name}"));
    }
}

fn parse_required_address<'a>(arguments: &mut impl Iterator<Item = &'a str>) -> Option<usize> {
    let Some(value) = arguments.next() else {
        write_raw(b"missing virtual address\n");
        return None;
    };
    match parse_usize(value) {
        Some(address) => Some(address),
        None => {
            write_raw(b"invalid virtual address\n");
            None
        }
    }
}

fn parse_usize(value: &str) -> Option<usize> {
    let (digits, radix) = match value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
    {
        Some(digits) => (digits, 16),
        None => (value, 10),
    };
    if digits.is_empty() {
        return None;
    }
    usize::from_str_radix(digits, radix).ok()
}

fn read_line(buffer: &mut [u8; LINE_CAPACITY], ignore_line_feed: &mut bool) -> usize {
    let mut length = 0usize;
    loop {
        let Some(byte) = crate::diagnostics::log::crash_console_read() else {
            spin_loop();
            continue;
        };
        if byte == b'\n' && *ignore_line_feed {
            *ignore_line_feed = false;
            continue;
        }
        match byte {
            b'\r' => {
                *ignore_line_feed = true;
                write_raw(b"\n");
                return length;
            }
            b'\n' => {
                *ignore_line_feed = false;
                write_raw(b"\n");
                return length;
            }
            0x08 | 0x7f if length != 0 => {
                length -= 1;
                write_raw(b"\x08 \x08");
            }
            0x15 => {
                while length != 0 {
                    length -= 1;
                    write_raw(b"\x08 \x08");
                }
            }
            0x20..=0x7e if length < buffer.len() => {
                *ignore_line_feed = false;
                buffer[length] = byte;
                length += 1;
                write_raw(&[byte]);
            }
            _ => {}
        }
    }
}

struct ConsoleWriter;

impl fmt::Write for ConsoleWriter {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        write_raw(text.as_bytes());
        Ok(())
    }
}

fn write_raw(bytes: &[u8]) {
    crate::diagnostics::log::crash_console_write(bytes);
}

fn write_line(arguments: fmt::Arguments<'_>) {
    write_line_start(arguments);
    write_raw(b"\n");
}

fn write_line_start(arguments: fmt::Arguments<'_>) {
    let _ = ConsoleWriter.write_fmt(arguments);
}

fn write_fragment(arguments: fmt::Arguments<'_>) {
    let _ = ConsoleWriter.write_fmt(arguments);
}
