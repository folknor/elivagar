#!/usr/bin/env python3
"""Regression tests for the pre-Cargo source-identity trigger.

The trigger's agreement with the build.rs recipe is pinned from the Rust side
(crates/lib/tests/source_identity.rs runs this module's reader and identity
function against fixture repositories). These cover the trigger's own
failure handling, which no Rust test reaches.
"""

from __future__ import annotations

import hashlib
import io
import pathlib
import sys
import unittest
from contextlib import redirect_stdout
from unittest.mock import patch

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))

import source_identity  # noqa: E402


def shared_file(extra: str) -> str:
    return (
        "untracked_content_cap = 1\n"
        'git_environment_prefix = "GIT_"\n'
        'git_metadata_directory = ".git"\n'
        "excluded_paths = []\n"
        'root_tree_exclusions = ["target"]\n'
        f"{extra}\n"
    )


class SourceIdentityTests(unittest.TestCase):
    def test_git_environment_cannot_redirect_a_checkout_read(self) -> None:
        inputs = source_identity.source_inputs()
        environment = {"GIT_DIR": "/elsewhere", "GIT_INDEX_FILE": "x", "PATH": "/bin"}
        with patch.dict(source_identity.os.environ, environment, clear=True):
            with patch.object(source_identity.subprocess, "run") as run:
                run.return_value.returncode = 0
                run.return_value.stdout = b"ok"
                self.assertEqual(
                    source_identity.git(pathlib.Path("checkout"), inputs, "status"), b"ok"
                )
        self.assertEqual(run.call_args.kwargs["env"], {"PATH": "/bin"})
        self.assertEqual(
            run.call_args.args[0], ["git", "-c", "core.excludesFile=", "status"]
        )

    def test_shared_inputs_read_the_same_in_any_layout(self) -> None:
        for names in (
            'excluded_suffixes = [".md", ".txt"]',
            'excluded_suffixes = [\n  ".md", # why\n  \'.txt\',\n]',
        ):
            inputs = source_identity.parse_source_inputs(shared_file(names))
            self.assertEqual(inputs.excluded_suffixes, (".md", ".txt"), names)

    def test_shared_inputs_refuse_a_shape_the_recipe_could_disagree_on(self) -> None:
        for case, extra in {
            "missing": "",
            "duplicate entry": 'excluded_suffixes = [".md", ".md"]',
            "not a string": "excluded_suffixes = [3]",
            "unknown key": 'excluded_suffixes = []\nsurprise = "x"',
            "duplicate key": "excluded_suffixes = []\nexcluded_suffixes = []",
            "not toml": "excluded_suffixes = [",
        }.items():
            with self.assertRaises(ValueError, msg=case):
                source_identity.parse_source_inputs(shared_file(extra))
        negative = shared_file("excluded_suffixes = []").replace(
            "untracked_content_cap = 1", "untracked_content_cap = -1"
        )
        with self.assertRaises(ValueError):
            source_identity.parse_source_inputs(negative)

    def test_checkout_without_git_metadata_is_not_described(self) -> None:
        inputs = source_identity.source_inputs()
        with patch.object(pathlib.Path, "stat", side_effect=FileNotFoundError):
            with patch.object(source_identity.subprocess, "run") as run:
                self.assertIsNone(
                    source_identity.identity(pathlib.Path("tarball"), inputs)
                )
        run.assert_not_called()

    def test_unreadable_inputs_use_a_unique_safe_fallback(self) -> None:
        output = io.StringIO()
        with patch.object(source_identity, "source_inputs", side_effect=ValueError("bad")):
            with patch.object(source_identity.time, "time_ns", return_value=123):
                with redirect_stdout(output):
                    self.assertEqual(source_identity.main(), 0)
        self.assertEqual(output.getvalue().strip(), "unidentifiable-123")

    def test_length_prefix_separates_adjacent_inputs(self) -> None:
        def digest(*parts: bytes) -> str:
            hasher = hashlib.sha256()
            for part in parts:
                source_identity._feed(hasher, part)
            return hasher.hexdigest()

        self.assertNotEqual(digest(b"ab", b"c"), digest(b"a", b"bc"))


if __name__ == "__main__":
    result = unittest.main(exit=False).result
    if result.wasSuccessful():
        print("source identity tests ok")
    else:
        raise SystemExit(1)
