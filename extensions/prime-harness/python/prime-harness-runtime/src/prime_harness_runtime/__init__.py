"""prime-harness kernel runtime for the pi-relay prime-harness extension (M2).

Loaded into the IPython kernel by prime-harness's bootstrap contribution
(sys.path insertion, not venv install — an M2 simplification of prime-agent's
uv-install-into-kernel-venv approach). Attaches the continual harness CRUD API
to the kernel's `rlm` object:

  rlm.harness                 -> HarnessState for THIS session (local scope)
  rlm.get_harness_state(...)  -> factory (global_=True for the global store)
  rlm.harness.create_memory(title, content, id=?, path=?, global_=?)
  rlm.harness.create_prompt_note / create_skill / create_subagent / create_role
  rlm.harness.update_* / delete_* / list / get / overview / snapshot
  rlm.harness.record_refinement(...)

Scope resolution (env, set by the host at kernel spawn):
  RLM_HARNESS_STATE_DIR        local store dir (per-session)
  RLM_GLOBAL_HARNESS_STATE_DIR global store dir
  RLM_SESSION_DIR              per-session dir (fallback for the local store)
"""

from __future__ import annotations

from prime_harness_runtime.harness import (
    HarnessEntry,
    HarnessKind,
    HarnessScope,
    HarnessState,
    RefinementEvent,
    get_harness_state,
)

__all__ = [
    "HarnessEntry",
    "HarnessKind",
    "HarnessScope",
    "HarnessState",
    "RefinementEvent",
    "get_harness_state",
    "harness",
]


def _make_local_state() -> HarnessState:
    """Construct the session-local state, degrading to in-memory on resolution errors."""
    try:
        return get_harness_state()
    except Exception as error:  # no local dir configured -> volatile store
        return HarnessState(in_memory=True, scope="local", local_write_error=str(error))


harness: HarnessState = _make_local_state()
