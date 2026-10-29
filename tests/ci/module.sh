#!/bin/sh
# SPDX-FileCopyrightText: 2026 roolrz
# SPDX-License-Identifier: Apache-2.0
set -eu
root=$(CDPATH='' cd -- "$(dirname "$0")/../.." && pwd)
cd "$root"
if [ -n "${KERNEL_CI_SUITE:-}" ]; then
    exec sh tests/ci/run.sh "$KERNEL_CI_SUITE"
fi
# Local component tests cover the same shards as this repository's PR workflow.
# aarch64-build is a convenience aggregate, not an additional matrix job.
for suite in quality scripts configurations aarch64-production aarch64-compact aarch64-qemu riscv64-qemu x86_64-build; do
    sh tests/ci/run.sh "$suite"
done
