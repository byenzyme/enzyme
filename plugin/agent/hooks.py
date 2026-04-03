"""Lifecycle hooks for the enzyme Hermes plugin.

Maps Hermes session events to enzyme CLI operations.
"""

import os
import subprocess

from . import setup


_ENZYME_MARKER = "<!-- enzyme:start -->"

# Context files Hermes loads into the system prompt, in priority order.
_CONTEXT_FILES = [".hermes.md", "HERMES.md", "AGENTS.md", "agents.md", "CLAUDE.md", "claude.md"]


def _vault_is_initialized() -> bool:
    """Check if the current directory has an enzyme index."""
    return os.path.exists(os.path.join(".enzyme", "enzyme.db"))


def _context_file_has_enzyme() -> bool:
    """Check if any context file already contains the enzyme section."""
    for name in _CONTEXT_FILES:
        if os.path.exists(name):
            try:
                with open(name) as f:
                    return _ENZYME_MARKER in f.read()
            except OSError:
                pass
    return False


def on_session_start(**kwargs) -> None:
    """Bootstrap enzyme and ensure the vault is indexed.

    Hermes blocks on this hook before the LLM loop starts.
    """
    if not setup.is_enzyme_available():
        setup.ensure_enzyme_installed()

    if not setup.is_enzyme_available():
        return

    if not setup.is_enzyme_current():
        setup.ensure_enzyme_installed()

    if not _vault_is_initialized():
        subprocess.run(["enzyme", "init"], capture_output=True, timeout=120)
    else:
        subprocess.run(["enzyme", "refresh", "--quiet"], capture_output=True, timeout=60)


def on_session_end(**kwargs) -> None:
    """Prompt the agent to capture session insights before closing.

    Hermes passes: session_id, completed, interrupted, model, platform.
    Only fires on completed sessions — interrupted ones may lack context.
    Returns context string that nudges the agent to review and capture.
    """
    if not setup.is_enzyme_available():
        return

    if not kwargs.get("completed", False):
        return  # interrupted or unknown — skip

    # No-op for now: the SKILL.md instructions handle capture decisions.
    # This hook exists as a registration point for future session-end
    # logic (e.g., auto-refresh after a productive session).
    subprocess.run(
        ["enzyme", "refresh", "--quiet"],
        capture_output=True,
        timeout=30,
    )


_skip_injection = False

# First-session instruction block — mirrors ENZYME_SECTION from the
# Rust side but stays concise for ephemeral injection.
_FIRST_SESSION_INSTRUCTIONS = """\
This is a personal knowledge vault indexed by enzyme.

Start by calling the enzyme_petri tool to see what's in the vault — it returns \
the main topics, activity trends, and thematic questions (catalysts). Use those \
catalysts to compose enzyme_catalyze queries for semantic search.

Workflow:
1. enzyme_petri (with --query if the user's message is specific) → get entities and catalysts
2. enzyme_catalyze using catalyst-inspired queries → get excerpts with file paths and dates
3. Present findings using the user's own words. Never expose tool names.

Use enzyme tools instead of grep/search_files for vault content — they find \
content by concept, not keyword."""


def pre_llm_call(**kwargs) -> dict:
    """First-session bridge: inject instructions until context file exists.

    Once enzyme init writes instructions to CLAUDE.md / AGENTS.md / .hermes.md,
    the system prompt carries them natively and this hook becomes a no-op.
    """
    global _skip_injection

    if _skip_injection:
        return {}

    if not setup.is_enzyme_available():
        return {}

    if _context_file_has_enzyme():
        _skip_injection = True
        return {}

    return {"context": _FIRST_SESSION_INSTRUCTIONS}
