#!/usr/bin/env python3
# SPDX-FileCopyrightText: 2026 roolrz
# SPDX-License-Identifier: Apache-2.0

"""Export kernel build provenance without making unchanged builds stale."""

import argparse
from datetime import datetime, timezone
import hashlib
import os
from pathlib import Path
import subprocess


ROOT = Path(__file__).resolve().parents[2]


def git(root, *args):
    result = subprocess.run(["git", "-C", str(root), *args], capture_output=True)
    return result.stdout if result.returncode == 0 else None


def source_state(root):
    revision = git(root, "rev-parse", "HEAD")
    if revision is None:
        return "unknown", b""
    status = git(root, "status", "--porcelain=v1", "--untracked-files=all")
    if status is None:
        raise RuntimeError("cannot determine Git working tree state")
    return revision.decode().strip(), status


def build_timestamp():
    epoch = os.environ.get("SOURCE_DATE_EPOCH")
    if epoch is not None and (not epoch.isascii() or not epoch.isdecimal()):
        raise ValueError("SOURCE_DATE_EPOCH must be a nonnegative Unix timestamp")
    now = datetime.now(timezone.utc) if epoch is None else datetime.fromtimestamp(int(epoch), timezone.utc)
    return now.strftime("%Y-%m-%dT%H:%M:%SZ")


def cargo_metadata(root):
    revision, status = source_state(root)
    print("cargo:rerun-if-env-changed=HYPER_BUILD_GIT_STATE")
    print("cargo:rerun-if-env-changed=SOURCE_DATE_EPOCH")
    print("cargo:rerun-if-changed=tools/build-version.py")
    if revision == "unknown":
        print("cargo:warning=Git metadata unavailable; kernel revision is unknown")
    else:
        # Track source edits even while the tree remains dirty. Avoid watching
        # the repository directory, which would recursively include build output.
        paths = git(root, "ls-files", "--cached", "--others", "--exclude-standard", "-z")
        if paths is None:
            raise RuntimeError("cannot enumerate Git build inputs")
        for name in sorted(set(paths.split(b"\0")) - {b""}):
            path = root / os.fsdecode(name)
            if path.exists():
                print(f"cargo:rerun-if-changed={path}")
        # Resolve worktree metadata through Git rather than assuming .git is a directory.
        for name in ("HEAD", "index", "refs", "packed-refs"):
            value = git(root, "rev-parse", "--path-format=absolute", "--git-path", name)
            if value is not None:
                path = Path(os.fsdecode(value).strip())
                if path.exists():
                    print(f"cargo:rerun-if-changed={path}")
    suffix = "-dirty" if status else ""
    print(f"cargo:rustc-env=HYPER_BUILD_REVISION={revision[:12]}{suffix}")
    print(f"cargo:rustc-env=HYPER_BUILD_TIMESTAMP={build_timestamp()}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--state", action="store_true")
    args = parser.parse_args()
    if args.state:
        # Make evaluates this once per invocation. New/deleted untracked files
        # also invalidate Cargo's cached build script, without changing timestamps
        # on a no-op build. Actual source contents are Cargo dependencies above.
        revision, status = source_state(ROOT)
        print(revision + "-" + hashlib.sha256(status).hexdigest())
    else:
        cargo_metadata(ROOT)


if __name__ == "__main__":
    main()
