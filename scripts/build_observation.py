#!/usr/bin/env python3
"""Print the token build.rs compares to decide its provenance is current.

brokkr runs this before every cargo call (`[cargo_env]` in brokkr.toml) and
exports the line as ELV_BUILD_OBSERVATION. build.rs declares
`rerun-if-env-changed` on it instead of rerunning unconditionally, because a
rerun build script rebuilds its package: every elivagar target used to
recompile on every cargo call over an unchanged tree.

The token is `<HEAD commit>:<git object id of the raw status bytes>`, read
with the same status arguments build.rs uses (build/git_status.rs) and with
every GIT_* variable removed, as build.rs does. It carries no interpretation:
the dirty rule lives only in build.rs, which computes this same token from the
bytes it stamps from and reruns again on the next build when they disagree.

Build-cost hazard: when a read fails this prints a value unique per call,
which is correct (provenance must never go stale) but makes every cargo call
rebuild the crate while every gate stays green. The `build-observation-stable`
script check runs this twice and fails on that.
"""

import os
import subprocess
import sys
import time

STATUS_ARGS = [
    "--no-optional-locks",
    "status",
    "--porcelain=v1",
    "-z",
    "--untracked-files=all",
    "--no-renames",
]


def git(args, stdin=None):
    environment = {k: v for k, v in os.environ.items() if not k.startswith("GIT_")}
    try:
        result = subprocess.run(
            ["git", *args],
            input=stdin,
            capture_output=True,
            env=environment,
            check=False,
        )
    except OSError:
        return None
    return result.stdout if result.returncode == 0 else None


def token():
    head = git(["rev-parse", "HEAD"])
    status = git(STATUS_ARGS)
    if head is None or status is None:
        return None
    oid = git(["hash-object", "--stdin"], stdin=status)
    head = head.decode("utf-8", "replace").strip()
    if oid is None or not head:
        return None
    return f"{head}:{oid.decode('utf-8', 'replace').strip()}"


def main():
    value = token()
    if value is None:
        print("build observation unavailable: a git read failed", file=sys.stderr)
        print(f"unidentifiable-{time.time_ns()}")
        return 0
    print(value)
    return 0


if __name__ == "__main__":
    sys.exit(main())
