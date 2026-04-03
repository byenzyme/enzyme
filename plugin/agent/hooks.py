"""Lifecycle hooks for the enzyme Hermes plugin.

Maps Hermes session events to enzyme CLI operations.
"""

import json
import os
import subprocess

from . import setup


def _vault_is_initialized() -> bool:
    """Check if the current directory has an enzyme index."""
    return os.path.exists(os.path.join(".enzyme", "enzyme.db"))


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


_cached_context = None


def pre_llm_call(**kwargs) -> dict:
    """Inject vault context on every turn.

    Runs petri once (first turn), caches the result, and injects it
    on every subsequent turn so the model always knows to use enzyme
    tools. ~2KB per turn.
    """
    global _cached_context

    if not setup.is_enzyme_available():
        return {}

    if _cached_context is not None:
        return {"context": _cached_context}

    user_message = kwargs.get("user_message", "")

    try:
        cmd = ["enzyme", "petri", "-n", "5"]
        if user_message and len(user_message.split()) > 3:
            cmd.extend(["--query", user_message])
        result = subprocess.run(
            cmd,
            capture_output=True,
            text=True,
            timeout=10,
        )
        if result.returncode != 0 or not result.stdout.strip():
            return {}

        petri = json.loads(result.stdout)
        compact = []
        for entity in petri.get("entities", []):
            catalysts = [c["text"] for c in entity.get("catalysts", [])[:3]]
            if catalysts:
                compact.append({
                    "entity": entity.get("name", ""),
                    "trend": entity.get("activity_trend", ""),
                    "catalysts": catalysts,
                })
        if compact:
            _cached_context = (
                "This is a personal knowledge vault indexed by enzyme. "
                "The user's notes, highlights, and thinking are searchable "
                "through the enzyme_petri and enzyme_catalyze tools — use "
                "those, not grep or search_files, to explore vault content.\n\n"
                "Active topics and catalyst questions (use as enzyme_catalyze queries):\n\n"
                + json.dumps(compact)
            )
            return {"context": _cached_context}
    except (subprocess.TimeoutExpired, FileNotFoundError, json.JSONDecodeError):
        pass

    return {}
