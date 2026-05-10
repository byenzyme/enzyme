#!/usr/bin/env python3
"""Prepare skill-ready Enzyme release-note context from git history.

This script deliberately does not classify commits or write release prose itself.
It fetches the full git commit messages for a release range and injects them into
an agent prompt that points at the project-local release-notes skill. The skill is
responsible for synthesizing user-facing notes instead of concatenating commits.
"""

from __future__ import annotations

import argparse
import datetime as dt
import pathlib
import re
import subprocess
import sys

ROOT = pathlib.Path(__file__).resolve().parents[1]
DEFAULT_SKILL_PATH = "docs/skills/weekly-release-notes/SKILL.md"


def run_git(args: list[str]) -> str:
    try:
        return subprocess.check_output(["git", *args], cwd=ROOT, text=True).strip()
    except subprocess.CalledProcessError as exc:
        raise SystemExit(f"git {' '.join(args)} failed with exit code {exc.returncode}") from exc


def current_version() -> str:
    cargo = ROOT / "Cargo.toml"
    match = re.search(r'^version\s*=\s*"([^"]+)"', cargo.read_text(), re.MULTILINE)
    if not match:
        raise SystemExit("Could not find workspace version in Cargo.toml")
    return f"v{match.group(1)}"


def latest_tag() -> str:
    return run_git(["describe", "--tags", "--abbrev=0"])


def collect_commit_log(rev_range: str) -> str:
    """Return full commit messages and touched-file stats for a revision range."""
    return run_git(["log", "--reverse", "--stat", "--format=fuller", rev_range])


def render_context(version: str, from_ref: str, to_ref: str, commit_log: str) -> str:
    """Render only the collected release context for cron/script injection."""
    today = dt.date.today().isoformat()
    title_version = version if version.startswith("v") else f"v{version}"
    normalized_log = commit_log.strip() or "No commits were found in this range."
    return (
        f"# Enzyme release-note source context\n\n"
        f"- Version: {title_version}\n"
        f"- Generated: {today}\n"
        f"- Git range: `{from_ref}..{to_ref}`\n"
        f"- Source repository: `{ROOT}`\n\n"
        f"## Full git commit history\n\n"
        f"```text\n{normalized_log}\n```\n"
    )


def render_prompt(
    version: str,
    from_ref: str,
    to_ref: str,
    commit_log: str,
    skill_path: str = DEFAULT_SKILL_PATH,
) -> str:
    """Render an agent prompt that injects full commit history into the skill."""
    context = render_context(version, from_ref, to_ref, commit_log)
    return (
        f"Use the release-notes skill at `{skill_path}` to draft Enzyme release notes.\n\n"
        f"The full git commit history is injected below. Review it as the source of truth. "
        f"Do not concatenate commits. Synthesize user-facing changes, merge related commits "
        f"into coherent themes, and omit internal-only noise unless it changes user-visible "
        f"risk, setup, reliability, integrations, retrieval quality, or release operations.\n\n"
        f"Rules:\n"
        f"- Stay grounded in the injected commit text and touched-file stats.\n"
        f"- Do not invent metrics, customer claims, security guarantees, or roadmap promises.\n"
        f"- Prefer concrete, legitimacy-building language over hype.\n"
        f"- Include maintainer review notes for ambiguous claims or missing evidence.\n"
        f"- Use the requested version and range exactly unless the git history shows they are wrong.\n\n"
        f"{context}"
    )


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Fetch full git commit history and inject it into the Enzyme release-notes skill prompt."
    )
    parser.add_argument("--from", dest="from_ref", default=None, help="Base ref/tag. Defaults to the latest reachable tag.")
    parser.add_argument("--to", dest="to_ref", default="HEAD", help="Target ref. Defaults to HEAD.")
    parser.add_argument("--version", default=None, help="Release version label. Defaults to workspace Cargo.toml version.")
    parser.add_argument("--output", "-o", type=pathlib.Path, help="Write prompt/context to this file instead of stdout.")
    parser.add_argument(
        "--skill-path",
        default=DEFAULT_SKILL_PATH,
        help=f"Skill path to reference in the generated prompt. Defaults to {DEFAULT_SKILL_PATH}.",
    )
    parser.add_argument(
        "--context-only",
        action="store_true",
        help="Emit only metadata and full commit history, for cron/script stdout injection into an already skill-loaded job.",
    )
    args = parser.parse_args()

    from_ref = args.from_ref or latest_tag()
    to_ref = args.to_ref
    version = args.version or current_version()
    commit_log = collect_commit_log(f"{from_ref}..{to_ref}")
    output = (
        render_context(version, from_ref, to_ref, commit_log)
        if args.context_only
        else render_prompt(version, from_ref, to_ref, commit_log, args.skill_path)
    )

    if args.output:
        output_path = args.output if args.output.is_absolute() else ROOT / args.output
        output_path.parent.mkdir(parents=True, exist_ok=True)
        output_path.write_text(output)
    else:
        sys.stdout.write(output)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
