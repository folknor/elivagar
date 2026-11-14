#!/usr/bin/env python3
"""Fail when the build-provenance trigger is not stable over an unchanged tree.

brokkr runs scripts/build_observation.py before every cargo call and exports
its output as ELV_BUILD_OBSERVATION; build.rs reruns, and elivagar rebuilds,
whenever that value changes. A trigger stuck on its unique-per-call fallback
makes every cargo call a full crate rebuild with nothing going red. This runs
the trigger twice, as brokkr would, and fails if either value is a fallback or
the two differ. It runs only the trigger: no cargo, no brokkr, no binary.
"""

import pathlib
import subprocess
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
TRIGGER = ROOT / "scripts" / "build_observation.py"


def run():
    result = subprocess.run(
        [sys.executable, str(TRIGGER)],
        cwd=ROOT,
        capture_output=True,
        text=True,
        check=False,
    )
    return result.returncode, result.stdout.strip(), result.stderr.strip()


def main():
    runs = [run(), run()]
    for code, out, err in runs:
        if code != 0 or not out or out.startswith("unidentifiable"):
            print(
                f"build observation is {out!r} (exit {code}); every cargo call "
                f"rebuilds elivagar until this is fixed.\nstderr:\n{err or '(empty)'}"
            )
            return 1
    if runs[0][1] != runs[1][1]:
        print(
            f"build observation differs between two runs ({runs[0][1]} then "
            f"{runs[1][1]}); every cargo call would rebuild elivagar. An edit "
            "racing this check also lands here; rerun to tell them apart."
        )
        return 1
    print("build observation stable")
    return 0


if __name__ == "__main__":
    sys.exit(main())
