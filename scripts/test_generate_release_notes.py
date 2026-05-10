#!/usr/bin/env python3
"""Tests for the release note prompt generator."""

from __future__ import annotations

import importlib.util
import pathlib
import sys
import unittest
from unittest import mock

SCRIPT = pathlib.Path(__file__).with_name("generate-release-notes.py")
spec = importlib.util.spec_from_file_location("generate_release_notes", SCRIPT)
generate_release_notes = importlib.util.module_from_spec(spec)
assert spec.loader is not None
sys.modules["generate_release_notes"] = generate_release_notes
spec.loader.exec_module(generate_release_notes)


class ReleaseNotesPromptTests(unittest.TestCase):
    def test_render_prompt_injects_full_commit_log_for_skill_guided_generation(self) -> None:
        commit_log = """commit abc123
Author: Example <example@example.com>
Date:   2026-05-10

    feat: preserve auth state on reinstall

    Keep existing tokens when the installer runs again.
"""

        prompt = generate_release_notes.render_prompt(
            version="v0.5.5",
            from_ref="v0.5.4",
            to_ref="HEAD",
            commit_log=commit_log,
            skill_path=".agents/skills/release/SKILL.md",
        )

        self.assertIn("Use the release skill at `.agents/skills/release/SKILL.md`", prompt)
        self.assertIn("v0.5.4..HEAD", prompt)
        self.assertIn("feat: preserve auth state on reinstall", prompt)
        self.assertIn("Keep existing tokens when the installer runs again.", prompt)
        self.assertIn("Do not concatenate commits", prompt)
        self.assertIn("## Full git commit history", prompt)

    def test_collect_commit_log_uses_fuller_messages_and_stat_not_local_classification(self) -> None:
        calls: list[list[str]] = []

        def fake_run_git(args: list[str]) -> str:
            calls.append(args)
            return "full commit log"

        with mock.patch.object(generate_release_notes, "run_git", fake_run_git):
            result = generate_release_notes.collect_commit_log("v0.5.4..HEAD")

        self.assertEqual(result, "full commit log")
        self.assertEqual(calls, [["log", "--reverse", "--stat", "--format=fuller", "v0.5.4..HEAD"]])


if __name__ == "__main__":
    unittest.main()
