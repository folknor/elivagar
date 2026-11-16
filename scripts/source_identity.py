#!/usr/bin/env python3
"""Print one line that changes whenever build.rs's source identity inputs change.

brokkr runs this before every cargo call (`[cargo_env]` in brokkr.toml) and
exports the line as ELV_SOURCE_IDENTITY. crates/lib/build.rs declares
`rerun-if-env-changed` on it instead of rerunning unconditionally, because a
rerun build script recompiles its package: every elivagar target used to
recompile on every cargo call over an unchanged tree.

This is a trigger, not the stamp. When build.rs reruns it computes the real
`source_xxh3_128` digest from content itself (crates/lib/build/
source_identity.rs); the two values are never compared, so this side uses
stdlib SHA-256 and only has to change whenever the recipe's inputs do. It
hashes what the recipe hashes - the HEAD tree, the `git diff HEAD` patch, the
contents of untracked non-ignored files with the same size cap, and
Cargo.lock - with the same git arguments and the same exclusions, read from
the shared data file crates/lib/build/source_inputs.toml. Content, never
merely paths: the porcelain-status token this replaced named an untracked
path without its bytes, so rewriting an already-dirty file left the stamp
stale. crates/lib/tests/source_identity.rs pins that this reader and the
recipe read the shared file identically and change on the same edits.

Build-cost hazard: a checkout without git metadata, a failed git read or an
unreadable shared file prints a value unique per call. That is correct
(provenance must never go stale) but makes every cargo call recompile the
crate while every gate stays green. The `source-identity-stable` script check
runs this twice and fails on that.
"""

from __future__ import annotations

import dataclasses
import functools
import hashlib
import os
import pathlib
import stat
import subprocess
import sys
import time
import tomllib

ROOT = pathlib.Path(__file__).resolve().parent.parent
INPUTS_PATH = ROOT / "crates" / "lib" / "build" / "source_inputs.toml"

# Kept identical to DIFF_ARGS and UNTRACKED_ARGS in
# crates/lib/build/source_identity.rs.
DIFF_ARGS = (
    "diff",
    "HEAD",
    "--binary",
    "--no-ext-diff",
    "--no-textconv",
    "--no-color",
    "--no-renames",
    "-U0",
)
UNTRACKED_ARGS = ("ls-files", "--others", "--exclude-standard", "-z")

INPUTS_UNREADABLE = "shared source inputs could not be read"


@dataclasses.dataclass(frozen=True)
class SourceInputs:
    untracked_content_cap: int
    git_environment_prefix: str
    git_metadata_directory: str
    excluded_suffixes: tuple[str, ...]
    excluded_paths: tuple[str, ...]
    root_tree_exclusions: tuple[str, ...]

    def excludes_path(self, relative: bytes) -> bool:
        return any(
            relative.endswith(os.fsencode(suffix)) for suffix in self.excluded_suffixes
        ) or any(relative == os.fsencode(path) for path in self.excluded_paths)

    def git_pathspecs(self) -> list[str]:
        """The exclusions as git pathspecs, as SourceInputs::git_pathspecs."""
        return (
            ["."]
            + [f":(exclude,glob)**/*{suffix}" for suffix in self.excluded_suffixes]
            + [f":(exclude,glob){path}" for path in self.excluded_paths]
        )


def _string(table: dict, name: str) -> str:
    value = table[name]
    if not isinstance(value, str):
        raise ValueError(f"shared source input {name} must be a string")
    return value


def _strings(table: dict, name: str) -> tuple[str, ...]:
    values = table[name]
    if not isinstance(values, list) or not all(isinstance(v, str) for v in values):
        raise ValueError(f"shared source input {name} must be an array of strings")
    if len(values) != len(set(values)):
        raise ValueError(f"shared source input {name} contains a duplicate entry")
    return tuple(values)


def parse_source_inputs(text: str) -> SourceInputs:
    """Read the shared inputs strictly, refusing what the Rust scanner refuses
    in shape: an unknown or missing key, a wrong type, a duplicate entry."""
    table = tomllib.loads(text)
    fields = {field.name for field in dataclasses.fields(SourceInputs)}
    unknown = sorted(set(table) - fields)
    if unknown:
        raise ValueError(f"shared source input {unknown[0]} is not a known key")
    missing = sorted(fields - set(table))
    if missing:
        raise ValueError(f"shared source input {missing[0]} is missing")
    cap = table["untracked_content_cap"]
    if isinstance(cap, bool) or not isinstance(cap, int) or cap < 0:
        raise ValueError(
            "shared source input untracked_content_cap must be a non-negative integer"
        )
    return SourceInputs(
        untracked_content_cap=cap,
        git_environment_prefix=_string(table, "git_environment_prefix"),
        git_metadata_directory=_string(table, "git_metadata_directory"),
        excluded_suffixes=_strings(table, "excluded_suffixes"),
        excluded_paths=_strings(table, "excluded_paths"),
        root_tree_exclusions=_strings(table, "root_tree_exclusions"),
    )


@functools.cache
def source_inputs() -> SourceInputs:
    return parse_source_inputs(INPUTS_PATH.read_text(encoding="utf-8"))


def git_environment(environment, inputs: SourceInputs) -> dict:
    """Keep git reads scoped to their cwd instead of ambient repository routing."""
    prefix = inputs.git_environment_prefix
    return {k: v for k, v in environment.items() if not k.startswith(prefix)}


def git(checkout: pathlib.Path, inputs: SourceInputs, *args: str) -> bytes | None:
    try:
        result = subprocess.run(
            ["git", "-c", "core.excludesFile=", *args],
            cwd=checkout,
            capture_output=True,
            env=git_environment(os.environ, inputs),
            check=False,
        )
    except OSError:
        return None
    return result.stdout if result.returncode == 0 else None


def _feed(hasher, data: bytes) -> None:
    """Length-prefixed, so one file's bytes cannot impersonate the next label."""
    hasher.update(len(data).to_bytes(8, "big") + data)


def _feed_file(hasher, path: pathlib.Path, inputs: SourceInputs) -> bool:
    try:
        metadata = os.lstat(path)
        if stat.S_ISLNK(metadata.st_mode):
            _feed(hasher, os.fsencode(os.readlink(path)))
            return True
        if not stat.S_ISREG(metadata.st_mode):
            return False
        if metadata.st_size <= inputs.untracked_content_cap:
            _feed(hasher, path.read_bytes())
            return True
        if metadata.st_mtime_ns < 0:
            return False
        seconds, nanoseconds = divmod(metadata.st_mtime_ns, 1_000_000_000)
        _feed(hasher, f"oversize:{metadata.st_size}:{seconds}.{nanoseconds:09}".encode())
        return True
    except OSError:
        return False


def _feed_tree(
    hasher, checkout: pathlib.Path, directory: pathlib.Path, inputs: SourceInputs
) -> bool:
    """An untracked nested checkout: every file below it, its own git metadata
    skipped, the shared exclusions applied at every depth."""
    files = []
    pending = [directory]
    while pending:
        current = pending.pop()
        try:
            with os.scandir(current) as entries:
                children = list(entries)
        except OSError:
            return False
        for entry in children:
            if current == directory and entry.name == inputs.git_metadata_directory:
                continue
            path = pathlib.Path(entry.path)
            relative = os.fsencode(path.relative_to(checkout).as_posix())
            if inputs.excludes_path(relative):
                continue
            try:
                metadata = os.lstat(path)
            except OSError:
                return False
            if stat.S_ISDIR(metadata.st_mode):
                pending.append(path)
            else:
                files.append((relative, path))
    for relative, path in sorted(files):
        hasher.update(b"file\0" + relative + b"\0")
        if not _feed_file(hasher, path, inputs):
            return False
    return True


def identity(root: pathlib.Path, inputs: SourceInputs) -> str | None:
    """The trigger value for the git checkout at `root`, or None when it cannot
    be described. A checkout without its own git metadata is None too: the
    recipe would tree-walk it, which this trigger does not repeat."""
    hasher = hashlib.sha256()
    try:
        (root / inputs.git_metadata_directory).stat()
    except OSError:
        return None
    pathspecs = inputs.git_pathspecs()
    tree = git(root, inputs, "rev-parse", "HEAD^{tree}")
    patch = git(root, inputs, *DIFF_ARGS, "--", *pathspecs)
    untracked = git(root, inputs, *UNTRACKED_ARGS, "--", *pathspecs)
    if tree is None or not tree.strip() or patch is None or untracked is None:
        return None
    hasher.update(b"tree\0" + tree.strip() + b"\0")
    hasher.update(b"worktree\0")
    _feed(hasher, patch)
    try:
        lock = (root / "Cargo.lock").read_bytes()
    except FileNotFoundError:
        lock = b"absent"
    except OSError:
        return None
    hasher.update(b"lockfile\0")
    _feed(hasher, lock)
    for name in sorted(n for n in untracked.split(b"\0") if n):
        hasher.update(b"untracked\0" + name + b"\0")
        path = root / os.fsdecode(name.rstrip(b"/"))
        if name.endswith(b"/"):
            if not _feed_tree(hasher, root, path, inputs):
                return None
        elif not _feed_file(hasher, path, inputs):
            return None
    return hasher.hexdigest()


def main() -> int:
    try:
        inputs = source_inputs()
    except (OSError, UnicodeError, ValueError) as error:
        print(f"source identity unavailable: {INPUTS_UNREADABLE}: {error}", file=sys.stderr)
        print(f"unidentifiable-{time.time_ns()}")
        return 0
    value = identity(ROOT, inputs)
    if value is None:
        print(
            f"source identity unavailable: git or source input unreadable at {ROOT}",
            file=sys.stderr,
        )
        print(f"unidentifiable-{time.time_ns()}")
        return 0
    print(value)
    return 0


if __name__ == "__main__":
    sys.exit(main())
