# SPDX-FileCopyrightText: 2026 roolrz
# SPDX-License-Identifier: Apache-2.0

# Kernel-owned build targets and standalone QEMU defaults. Product composition
# includes this data without importing kernel build recipes or duplicating it.
ARCH ?= aarch64
KERNEL_PROFILE := kernel
ifeq ($(ARCH),aarch64)
KERNEL_TARGET := aarch64-unknown-none-softfloat
QEMU ?= qemu-system-aarch64
QEMU_CPU ?= max
QEMU_MACHINE ?= virt,virtualization=on,gic-version=3,dtb-randomness=on
QEMU_BOOTARGS ?= earlycon=pl011,mmio32,0x09000000
else ifeq ($(ARCH),riscv64)
KERNEL_TARGET := riscv64imac-unknown-none-elf
QEMU ?= qemu-system-riscv64
QEMU_CPU ?= rv64
QEMU_MACHINE ?= virt
QEMU_BOOTARGS ?= earlycon=uart8250,mmio,0x10000000
else ifeq ($(ARCH),x86_64)
KERNEL_TARGET := x86_64-unknown-none
QEMU ?= qemu-system-x86_64
QEMU_CPU ?= max
QEMU_MACHINE ?= q35,accel=tcg
QEMU_BOOTARGS ?= earlycon=uart8250,io,0x3f8
else
$(error unsupported ARCH '$(ARCH)')
endif
QEMU_CPUS ?= 4
QEMU_MEMORY ?= 1G
