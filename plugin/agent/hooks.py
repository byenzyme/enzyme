"""Lifecycle hooks for the enzyme Hermes plugin.

Maps Hermes session events to enzyme CLI operations.
"""

import os
import subprocess

from . import setup


def _vault_is_initialized() -> bool:
    """Check if the current directory has an enzyme index."""
    return os.path.exists(os.path.join(".enzyme", "enzyme.db"))


def on_session_start(**kwargs) -> None:
    """Bootstrap the enzyme binary if missing.

    Called at the start of every Hermes session. Only handles binary
    install/update — vault init and refresh happen in pre_llm_call
    where Hermes blocks before the LLM sees anything.
    """
    if not setup.is_enzyme_available():
        setup.ensure_enzyme_installed()

    if not setup.is_enzyme_available():
        return  # install failed — tools will be gated by check_fn

    if not setup.is_enzyme_current():
        setup.ensure_enzyme_installed()


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


def pre_llm_call(**kwargs) -> dict:
    """Inject vault context on the first turn of a session.

    Hermes passes: session_id, user_message, conversation_history,
    is_first_turn, model, platform.

    Only fires on the first turn to seed the model with vault context.
    After that, the model uses enzyme tools explicitly when it needs
    to go deeper — running petri on every turn would waste tokens and
    add latency for no benefit.

    If the user's first message has a clear direction, we pass it as
    a query so petri ranks by relevance. If it's open-ended, we run
    petri without a query for the full picture.
    """
    if not setup.is_enzyme_available():
        return {}

    if not kwargs.get("is_first_turn", False):
        return {}

    # First-ever session: init the vault before querying
    if not _vault_is_initialized():
        try:
            subprocess.run(
                ["enzyme", "init"],
                capture_output=True,
                timeout=120,
            )
        except (subprocess.TimeoutExpired, FileNotFoundError):
            return {}
    else:
        # Subsequent sessions: fast refresh
        try:
            subprocess.run(
                ["enzyme", "refresh", "--quiet"],
                capture_output=True,
                timeout=60,
            )
        except (subprocess.TimeoutExpired, FileNotFoundError):
            pass

    user_message = kwargs.get("user_message", "")

    try:
        cmd = ["enzyme", "petri"]
        if user_message and len(user_message.split()) > 3:
            cmd.extend(["--query", user_message])
        result = subprocess.run(
            cmd,
            capture_output=True,
            text=True,
            timeout=10,
        )
        if result.returncode == 0 and result.stdout.strip():
            return {"context": result.stdout.strip()}
    except (subprocess.TimeoutExpired, FileNotFoundError):
        pass

    return {}
