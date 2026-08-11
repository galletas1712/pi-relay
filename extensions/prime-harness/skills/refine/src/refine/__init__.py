"""Prime harness refine skill: continual harness refinement from the kernel.

PROVENANCE: ported from prime-agent's packages/coding-agent/skills/refine/
src/refine/__init__.py. Only the host-bridge import was adapted:
PA imports `from rlm import host_request`; pi-relay's M1 kernel exposes the
same bridge as `prime_rlm_runtime.host_request`.

Refinement runs host-side (the same implementation as /refine); these
functions are thin typed wrappers over the generic host bridge. They only
work inside a prime-rlm IPython kernel with prime-harness loaded.
"""

from __future__ import annotations

from typing import Any

from prime_rlm_runtime import host_request


async def status() -> dict[str, Any]:
    """Read current refine state.

    Returns a dict with `pending` (whether a requested refine is queued or a
    finished plan awaits application) and `in_flight` (whether a refine is
    currently planning or applying).
    """
    return await host_request("refine.status")


async def run(
    instructions: str | None = None,
    global_: bool = False,
) -> dict[str, Any]:
    """Schedule continual harness refinement.

    Planning runs in the background and does not block the conversation; the
    fast apply phase runs at the next turn boundary. Returns
    `{"scheduled": True}`, or `{"scheduled": False, "reason": ...}` when
    refinement cannot start. Optional `instructions` focus the refinement on
    a specific observation. Set `global_=True` to target the global
    (cross-session) harness store; omit for local (session-scoped) refinement.
    """
    if instructions is not None and not isinstance(instructions, str):
        raise TypeError(
            f"instructions must be str or None, got {type(instructions).__name__}"
        )
    if not isinstance(global_, bool):
        raise TypeError(f"global_ must be bool, got {type(global_).__name__}")
    payload: dict[str, Any] = {}
    if instructions is not None:
        payload["instructions"] = instructions
    if global_:
        payload["global"] = True
    return await host_request("refine.run", payload)
