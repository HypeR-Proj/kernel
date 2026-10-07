#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2026 roolrz
# SPDX-License-Identifier: Apache-2.0

"""Exercise actual PCI DMA through QEMU's architectural SMMUv3 model."""

import argparse
from pathlib import Path
import subprocess
import tempfile
import time


MARKERS = (
    "SMMUv3 stage-2 active:",
    "SMMUv3 PCI routing:",
    "SMMUv3 bidirectional DMA and separate domains passed",
    "SMMUv3 unmapped and read/write permission faults passed",
    "SMMUv3 revoke/remap and stream reassignment passed",
    "SMMUv3 command and event queue wrap passed",
    "SMMUv3 DMA isolation acceptance passed",
    "SMMUv3 pre-enable event drain passed",
    "SMMUv3 IRQ worker, per-stream quarantine and fault flood suppression passed",
    "kernel self-tests completed",
)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("qemu")
    parser.add_argument("image")
    parser.add_argument("--cpus", type=int, default=4)
    parser.add_argument("--timeout", type=int, default=180)
    parser.add_argument("--log", type=Path, default=Path("target/smmuv3-qemu.log"))
    parser.add_argument("--trace", type=Path, help="optional QEMU SMMU register/translation trace")
    parser.add_argument("--failure", choices=("gerror", "timeout"), default="gerror")
    args = parser.parse_args()
    if args.cpus < 1 or args.timeout < 1:
        parser.error("cpus and timeout must be positive")
    args.log.parent.mkdir(parents=True, exist_ok=True)
    command = [
        args.qemu, "-machine",
        "virt,virtualization=on,gic-version=3,iommu=smmuv3,default-bus-bypass-iommu=off",
        "-cpu", "max", "-smp", str(args.cpus), "-m", "1G",
        "-nodefaults", "-display", "none", "-monitor", "none",
        "-serial", "stdio", "-no-reboot", "-kernel", args.image,
        "-append", f"earlycon=pl011,mmio32,0x09000000 smmu-failure-test={args.failure}",
    ]
    for slot in (1, 2, 3):
        command += ["-device", f"edu,addr={slot}.0,dma_mask=0xffffffffffffffff"]
    if args.trace:
        command += ["-trace", f"enable=smmu*,file={args.trace}"]
    failure_marker = (
        "SMMUv3 no-IRQ timeout, worker wake and pinned backing passed"
        if args.failure == "timeout" else
        "SMMUv3 global-error IRQ, abort containment and pinned backing passed"
    )
    markers = (*MARKERS, failure_marker)
    with tempfile.TemporaryDirectory(prefix="hyper-smmuv3-") as temporary, args.log.open("wb") as output:
        initrd = Path(temporary) / "empty.cpio"
        subprocess.run(["sh", str(Path(__file__).with_name("empty-initramfs.sh")), str(initrd)], check=True)
        command += ["-initrd", str(initrd)]
        process = subprocess.Popen(command, stdin=subprocess.DEVNULL,
                                   stdout=output, stderr=subprocess.STDOUT)
        try:
            deadline = time.monotonic() + args.timeout
            text = ""
            while time.monotonic() < deadline:
                text = args.log.read_text(errors="replace")
                if any(marker in text for marker in ("HypeR: fatal", "HypeR crash", "panicked", "initialization failed", "preparation failed")):
                    raise RuntimeError(f"kernel failed; see {args.log}\n{text[-8000:]}")
                if all(marker in text for marker in markers):
                    print(f"SMMUv3 DMA isolation passed ({args.cpus} CPUs, {args.failure}); log: {args.log}")
                    return
                if process.poll() is not None:
                    raise RuntimeError(f"QEMU exited {process.returncode}; see {args.log}\n{text[-8000:]}")
                time.sleep(0.2)
            missing = [marker for marker in markers if marker not in text]
            raise TimeoutError(f"missing {missing}; see {args.log}\n{text[-8000:]}")
        finally:
            if process.poll() is None:
                process.terminate()
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()


if __name__ == "__main__":
    main()
