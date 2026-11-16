#!/usr/bin/env python3
"""Fail when the source-identity trigger is not stable over an unchanged tree.

brokkr runs scripts/source_identity.py before every cargo call and exports its
output as ELV_SOURCE_IDENTITY; crates/lib/build.rs reruns, and elivagar
recompiles, whenever that value changes. A trigger stuck on its
unique-per-call fallback makes every cargo call a full crate rebuild with
nothing going red. The most likely way there on a normal tree is the trigger
failing to read crates/lib/build/source_inputs.toml, but any fallback branch
costs the same. This runs the trigger twice, as brokkr would, and fails if
either value is a fallback or the two differ, printing the trigger's stderr so
the cause is visible. It runs only the trigger: no cargo, no brokkr, no binary.
"""

import os
import pathlib
import subprocess
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
TRIGGER = ROOT / "scripts" / "source_identity.py"
INPUTS_UNREADABLE = "shared source inputs could not be read"


def run():
    # Ambient Git routing removed, as brokkr's own environment would not carry
    # a hook's GIT_DIR into the check; the trigger strips it again itself.
    environment = {k: v for k, v in os.environ.items() if not k.startswith("GIT_")}
    result = subprocess.run(
        [sys.executable, str(TRIGGER)],
        cwd=ROOT,
        env=environment,
        capture_output=True,
        text=True,
        check=False,
    )
    return result.returncode, result.stdout.strip(), result.stderr.strip()


def main():
    runs = [run(), run()]
    for code, out, err in runs:
        if code != 0 or not out or out.startswith("unidentifiable"):
            if INPUTS_UNREADABLE in err:
                cause = "the trigger could not read crates/lib/build/source_inputs.toml"
            else:
                cause = "the trigger could not describe this checkout"
            print(
                f"source identity is {out!r} (exit {code}): {cause}; every cargo "
                f"call recompiles elivagar until this is fixed.\n"
                f"stderr:\n{err or '(empty)'}"
            )
            return 1
    if runs[0][1] != runs[1][1]:
        print(
            f"source identity differs between two runs ({runs[0][1]} then "
            f"{runs[1][1]}); every cargo call would recompile elivagar. An edit "
            "racing this check also lands here; rerun to tell them apart."
        )
        return 1
    print("source identity stable")
    return 0


if __name__ == "__main__":
    sys.exit(main())
