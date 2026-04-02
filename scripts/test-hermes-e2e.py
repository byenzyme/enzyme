#!/usr/bin/env python3
"""End-to-end test for enzyme plugin in Hermes.

Runs multi-turn conversations through Hermes CLI to verify:
- Implicit enzyme tool invocation (model decides to use enzyme without being told)
- pre_llm_call hook context injection
- Multi-turn continuity (resume sessions)
- Organic search patterns (catalyze triggered by conversational queries)

Usage:
    source /tmp/hermes-venv/bin/activate
    cd ~/obsidian
    python ~/src/enzyme-rust/scripts/test-hermes-e2e.py
"""

import json
import os
import re
import subprocess
import sys
import time
from dataclasses import dataclass, field

HERMES = "hermes"
PROVIDER = "openai-codex"
MAX_TURNS = 10
TIMEOUT = 120


@dataclass
class TurnResult:
    query: str
    response: str
    session_id: str
    tools_called: list[str] = field(default_factory=list)
    duration: float = 0.0
    error: str | None = None


def run_turn(query: str, session_id: str | None = None, verbose: bool = False) -> TurnResult:
    """Run a single conversation turn through Hermes CLI."""
    cmd = [
        HERMES, "chat",
        "-q", query,
        "--provider", PROVIDER,
        "-Q",  # quiet mode: suppress banner, spinner, tool previews
        "--max-turns", str(MAX_TURNS),
    ]
    if verbose:
        cmd.append("-v")
    if session_id:
        cmd.extend(["--resume", session_id])

    start = time.time()
    result = subprocess.run(
        cmd,
        capture_output=True,
        text=True,
        timeout=TIMEOUT,
        env={**os.environ, "NO_COLOR": "1"},
    )
    duration = time.time() - start

    output = result.stdout + result.stderr

    # Extract session ID
    sid_match = re.search(r"session_id:\s*(\S+)", output)
    sid = sid_match.group(1) if sid_match else session_id or "unknown"

    # Extract actual tool calls — look for the execution markers, not toolset listings.
    # Quiet mode shows: ┊ ⚡ enzyme_xx  (truncated tool name + timing)
    # Verbose mode shows: Tool call: enzyme_xxx or similar invocation lines.
    # Exclude lines that are toolset listings (contain commas listing multiple tools).
    tools = []
    for line in output.split("\n"):
        # Skip toolset listing lines
        if "Enabled toolset" in line or "Final tool selection" in line or "Loaded" in line:
            continue
        # Match actual invocation markers (⚡ prefix in quiet mode)
        # Tool names may be truncated in quiet output (e.g. "enzyme_ca" for "enzyme_catalyze")
        if "⚡" in line:
            match = re.search(r"enzyme_(\w+)", line)
            if match:
                suffix = match.group(1)
                # Expand truncated names
                full_names = {
                    "pe": "enzyme_petri", "pet": "enzyme_petri", "petr": "enzyme_petri", "petri": "enzyme_petri",
                    "ca": "enzyme_catalyze", "cat": "enzyme_catalyze", "cata": "enzyme_catalyze", "catalyze": "enzyme_catalyze",
                    "re": "enzyme_refresh", "ref": "enzyme_refresh", "refr": "enzyme_refresh", "refresh": "enzyme_refresh",
                    "st": "enzyme_status", "sta": "enzyme_status", "stat": "enzyme_status", "status": "enzyme_status",
                    "in": "enzyme_init", "ini": "enzyme_init", "init": "enzyme_init",
                }
                tools.append(full_names.get(suffix, f"enzyme_{suffix}"))
    # Deduplicate while preserving order
    seen = set()
    tools_unique = []
    for t in tools:
        if t not in seen:
            seen.add(t)
            tools_unique.append(t)

    # Extract the actual response (last block before session_id line)
    response = result.stdout.strip()
    # Remove session_id line
    response = re.sub(r"\n?session_id:.*$", "", response).strip()

    error = None
    if result.returncode != 0 and "API call failed" in output:
        error = re.search(r"Error:.*$", output, re.MULTILINE)
        error = error.group(0) if error else f"exit code {result.returncode}"

    return TurnResult(
        query=query,
        response=response,
        session_id=sid,
        tools_called=tools_unique,
        duration=duration,
        error=error,
    )


def print_result(turn: int, r: TurnResult):
    tools_str = ", ".join(r.tools_called) if r.tools_called else "(none)"
    status = "FAIL" if r.error else "OK"
    print(f"\n{'='*70}")
    print(f"Turn {turn}: [{status}] ({r.duration:.1f}s) tools: {tools_str}")
    print(f"  Query: {r.query}")
    if r.error:
        print(f"  Error: {r.error}")
    else:
        # Truncate long responses
        lines = r.response.split("\n")
        preview = "\n  ".join(lines[:8])
        if len(lines) > 8:
            preview += f"\n  ... ({len(lines) - 8} more lines)"
        print(f"  Response:\n  {preview}")


# ---------------------------------------------------------------------------
# Test scenarios
# ---------------------------------------------------------------------------

def test_organic_exploration():
    """Scenario 1: Organic vault exploration.

    Start with a vague personal question. The model should decide to use
    enzyme_petri or enzyme_catalyze without being told to.
    """
    print("\n" + "#" * 70)
    print("# Scenario 1: Organic vault exploration")
    print("#" * 70)

    turns = [
        "What have I been thinking about lately?",
        "That's interesting — tell me more about the tension between craft and speed. What have I written about that?",
        "Are there any notes where I've changed my mind about something?",
    ]

    session_id = None
    results = []
    for i, query in enumerate(turns, 1):
        r = run_turn(query, session_id=session_id, verbose=(i == 1))
        session_id = r.session_id
        results.append(r)
        print_result(i, r)
        if r.error:
            print("  Stopping scenario due to error.")
            break

    return results


def test_semantic_search_flow():
    """Scenario 2: Deep semantic search.

    Ask conceptual questions that should trigger enzyme_catalyze.
    """
    print("\n" + "#" * 70)
    print("# Scenario 2: Semantic search flow")
    print("#" * 70)

    turns = [
        "I've been feeling stuck on a project — have I written about this kind of feeling before?",
        "What about the relationship between urgency and ambition? I feel like I've explored that tension somewhere.",
    ]

    session_id = None
    results = []
    for i, query in enumerate(turns, 1):
        r = run_turn(query, session_id=session_id, verbose=True)
        session_id = r.session_id
        results.append(r)
        print_result(i, r)
        if r.error:
            break

    return results


def test_context_injection():
    """Scenario 3: Verify pre_llm_call hook.

    Send a message and check verbose output for signs of context injection.
    The pre_llm_call hook runs enzyme petri --query with the user message.
    """
    print("\n" + "#" * 70)
    print("# Scenario 3: Context injection (pre_llm_call hook)")
    print("#" * 70)

    r = run_turn(
        "How does the concept of dignity show up in my thinking about AI tools?",
        verbose=True,
    )
    print_result(1, r)

    # Check if the response references vault content without explicit tool calls
    has_vault_refs = any(
        marker in r.response.lower()
        for marker in ["vault", "note", "wrote", "written", "entry", "your"]
    )

    print(f"\n  Vault-aware response: {'yes' if has_vault_refs else 'no'}")
    print(f"  Explicit tool calls: {r.tools_called}")

    return [r]


def test_status_and_refresh():
    """Scenario 4: Operational commands.

    Indirectly trigger status/refresh by asking about vault health.
    """
    print("\n" + "#" * 70)
    print("# Scenario 4: Operational (status/refresh)")
    print("#" * 70)

    turns = [
        "Is my vault index up to date? How many documents do I have?",
        "Can you refresh the index and check again?",
    ]

    session_id = None
    results = []
    for i, query in enumerate(turns, 1):
        r = run_turn(query, session_id=session_id)
        session_id = r.session_id
        results.append(r)
        print_result(i, r)
        if r.error:
            break

    return results


# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------

def main():
    # Verify we're in the vault directory
    if not os.path.exists(".enzyme/enzyme.db"):
        print("Error: Run this script from your vault directory (where .enzyme/enzyme.db exists)")
        print(f"  Current dir: {os.getcwd()}")
        sys.exit(1)

    # Verify hermes is available
    try:
        subprocess.run([HERMES, "--version"], capture_output=True, check=True, timeout=10)
    except (FileNotFoundError, subprocess.CalledProcessError):
        print("Error: hermes not found. Activate the venv first:")
        print("  source /tmp/hermes-venv/bin/activate")
        sys.exit(1)

    print("Enzyme + Hermes E2E Test")
    print(f"  Vault: {os.getcwd()}")
    print(f"  Provider: {PROVIDER}")
    print(f"  Max turns per query: {MAX_TURNS}")

    all_results = []

    scenarios = [
        ("Organic exploration", test_organic_exploration),
        ("Semantic search", test_semantic_search_flow),
        ("Context injection", test_context_injection),
        ("Operational", test_status_and_refresh),
    ]

    # Allow selecting specific scenarios via CLI args
    if len(sys.argv) > 1:
        selected = [int(x) for x in sys.argv[1:]]
        scenarios = [(n, f) for i, (n, f) in enumerate(scenarios, 1) if i in selected]

    for name, fn in scenarios:
        try:
            results = fn()
            all_results.extend(results)
        except subprocess.TimeoutExpired:
            print(f"\n  TIMEOUT in scenario: {name}")
        except KeyboardInterrupt:
            print("\n  Interrupted.")
            break

    # Summary
    print("\n" + "=" * 70)
    print("SUMMARY")
    print("=" * 70)

    total = len(all_results)
    passed = sum(1 for r in all_results if not r.error)
    tools_used = set()
    for r in all_results:
        tools_used.update(r.tools_called)

    print(f"  Turns: {total} ({passed} passed, {total - passed} failed)")
    print(f"  Enzyme tools invoked: {', '.join(sorted(tools_used)) or '(none)'}")
    print(f"  Total time: {sum(r.duration for r in all_results):.1f}s")

    # Check which tools were never invoked
    expected_tools = {"enzyme_petri", "enzyme_catalyze", "enzyme_status", "enzyme_refresh"}
    missing = expected_tools - tools_used
    if missing:
        print(f"  Tools never invoked: {', '.join(sorted(missing))}")

    return 0 if passed == total else 1


if __name__ == "__main__":
    sys.exit(main())
