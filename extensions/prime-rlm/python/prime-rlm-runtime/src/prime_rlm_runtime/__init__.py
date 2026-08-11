"""Kernel-side rlm shim for the pi-relay prime-rlm extension.

Port of prime-agent-runtime's rlm module (M3: admission-async semantics),
minus harness/mcp/find_models. Provides `rlm` in the kernel namespace with:
  - handle = await rlm.run(prompt, name=?, model=?) -> RLMSpawnHandle at ADMISSION
    ({rlm_child_id, name, session_dir, model}); the handle never contains the
    answer — results arrive later as messages (agent_message wakeups) or files.
  - await rlm(prompt, ...)                    -> alias for run()
  - await rlm.list_subagents()                -> live child registry dataclasses
  - await rlm.delete_subagent(target)         -> cancel/remove a child
  - await rlm.snapshot_save()/snapshot_restore() -> scheduled kernel-state ops
"""

from __future__ import annotations

import asyncio
import sys
import types
from dataclasses import dataclass
from pathlib import Path
from typing import Any

try:
    from ipykernel.comm import Comm
except Exception:  # pragma: no cover - depends on ipykernel version
    Comm = None  # type: ignore[assignment]

try:
    from IPython import get_ipython
except Exception:  # pragma: no cover - only available in kernels
    get_ipython = None  # type: ignore[assignment]

HOST_COMM_TARGET = "host.request"


def _install_control_comm_handlers() -> None:
    """Let comm replies arrive on the control channel during an execute_request.

    Without this, a host request made from inside a running cell (host replies
    via the control channel while the shell channel is busy executing this cell)
    deadlocks: ipykernel does not dispatch comm_msg on the control thread by
    default.
    """
    if get_ipython is None:
        return
    shell = get_ipython()
    kernel = getattr(shell, "kernel", None)
    comm_manager = getattr(kernel, "comm_manager", None)
    control_handlers = getattr(kernel, "control_handlers", None)
    if comm_manager is None or not isinstance(control_handlers, dict):
        return
    control_handlers.setdefault("comm_msg", comm_manager.comm_msg)
    control_handlers.setdefault("comm_close", comm_manager.comm_close)


async def host_request(request_type: str, payload: dict[str, Any] | None = None) -> dict[str, Any]:
    """Send a typed request to the host and await its reply.

    Kernel side of the generic host bridge: the TypeScript host dispatches on
    the request type. Raises RuntimeError when the host reports an error or when
    no handler for the type is registered in this session.
    """
    if not isinstance(request_type, str) or not request_type:
        raise TypeError("request_type must be a non-empty str")
    if payload is not None and not isinstance(payload, dict):
        raise TypeError(f"payload must be a dict or None, got {type(payload).__name__}")
    if Comm is None:
        raise RuntimeError("Jupyter comm support is unavailable in this kernel")
    _install_control_comm_handlers()

    loop = asyncio.get_running_loop()
    future: asyncio.Future[dict[str, Any]] = loop.create_future()
    comm = Comm(target_name=HOST_COMM_TARGET, primary=False)

    def _on_msg(msg: dict[str, Any]) -> None:
        content = msg.get("content", {})
        reply = content.get("data", {}) if isinstance(content, dict) else {}
        if not isinstance(reply, dict):
            return

        status = reply.get("status")
        if status == "ok":
            # Handler result is nested under "value" so a payload "status" key
            # (e.g. a subagent entry's "completed") cannot collide with the envelope.
            value = reply.get("value", {})

            def _resolve_result() -> None:
                if not future.done():
                    future.set_result(value)
                    comm.close()

            loop.call_soon_threadsafe(_resolve_result)
            return
        if status == "error":
            message = reply.get("error") or f"host request {request_type} failed"

            def _resolve_error() -> None:
                if not future.done():
                    future.set_exception(RuntimeError(str(message)))
                    comm.close()

            loop.call_soon_threadsafe(_resolve_error)
            return

        unexpected = f"host request {request_type} returned unexpected status: {status!r}"

        def _resolve_unexpected() -> None:
            if not future.done():
                future.set_exception(RuntimeError(unexpected))
                comm.close()

        loop.call_soon_threadsafe(_resolve_unexpected)

    comm.on_msg(_on_msg)
    # request_type goes last so a payload "type" key cannot reroute the request.
    comm.open(data={**(payload or {}), "type": request_type})
    return await future


@dataclass(frozen=True)
class RLMSpawnHandle:
    """Returned by rlm() at admission; never contains the child's answer."""

    rlm_child_id: str
    name: str
    session_dir: Path
    model: str | None
    # M7: subagent role name when spawned via rlm(..., role="...").
    role: str | None = None

    def __getitem__(self, key: str) -> Any:
        return getattr(self, key)


@dataclass(frozen=True)
class RLMSubagent:
    rlm_child_id: str
    active_session_id: str | None
    session_id: str | None
    session_name: str
    session_dir: Path
    status: str
    result_preview: str | None
    # M7: subagent role name when spawned via rlm(..., role="...").
    role: str | None = None

    def __getitem__(self, key: str) -> Any:
        return getattr(self, key)


def _spawn_handle_from_payload(payload: Any) -> RLMSpawnHandle:
    if not isinstance(payload, dict):
        raise RuntimeError("rlm.run returned an invalid spawn handle")
    child_id = payload.get("rlm_child_id")
    name = payload.get("name")
    session_dir = payload.get("session_dir")
    model = payload.get("model")
    if not isinstance(child_id, str) or not child_id:
        raise RuntimeError("rlm.run reply is missing rlm_child_id")
    if not isinstance(name, str) or not name:
        raise RuntimeError("rlm.run reply is missing name")
    if not isinstance(session_dir, str) or not session_dir:
        raise RuntimeError("rlm.run reply is missing session_dir")
    role = payload.get("role")
    return RLMSpawnHandle(
        rlm_child_id=child_id,
        name=name,
        session_dir=Path(session_dir),
        model=model if isinstance(model, str) else None,
        role=role if isinstance(role, str) else None,
    )


def _subagent_from_payload(payload: Any, operation: str = "rlm.list_subagents") -> RLMSubagent:
    if not isinstance(payload, dict):
        raise RuntimeError(f"{operation} returned an invalid subagent entry")
    child_id = payload.get("rlm_child_id")
    session_id = payload.get("session_id")
    session_name = payload.get("session_name")
    session_dir = payload.get("session_dir")
    status = payload.get("status")
    result_preview = payload.get("result_preview")
    if not isinstance(child_id, str) or not child_id:
        raise RuntimeError(f"{operation} entry is missing rlm_child_id")
    if session_id is not None and not isinstance(session_id, str):
        raise RuntimeError(f"{operation} entry has invalid session_id")
    if not isinstance(session_name, str) or not session_name:
        raise RuntimeError(f"{operation} entry is missing session_name")
    if not isinstance(session_dir, str) or not session_dir:
        raise RuntimeError(f"{operation} entry is missing session_dir")
    if status not in {"running", "completed", "error"}:
        raise RuntimeError(f"{operation} entry has invalid status")
    active_session_id = payload.get("active_session_id")
    role = payload.get("role")
    return RLMSubagent(
        rlm_child_id=child_id,
        active_session_id=active_session_id if isinstance(active_session_id, str) else None,
        session_id=session_id,
        session_name=session_name,
        session_dir=Path(session_dir),
        status=status,
        result_preview=result_preview if isinstance(result_preview, str) else None,
        role=role if isinstance(role, str) else None,
    )


async def run(prompt: str, **kwargs: Any) -> RLMSpawnHandle:
    """Spawn a recursive child agent session; returns its handle at admission.

    The child runs concurrently. The handle never contains the child's answer:
    results arrive later as delivered messages (agent_message wakeups when
    prime-comms is loaded, otherwise a host result notice) or via files.

    ``model`` selects a child model via exact ``provider/model`` selector;
    ``name`` gives the child session a display name; ``role`` (M7) applies a
    packaged subagent role (SKILL.md instructions + preloaded skills inlined
    into the child prompt, plus the role's model/effort/max-tokens policy —
    an explicit ``model=`` kwarg wins over the role's).
    """
    if not isinstance(prompt, str):
        raise TypeError(f"prompt must be str, got {type(prompt).__name__}")
    payload = await host_request("rlm.run", {"prompt": prompt, "kwargs": kwargs})
    return _spawn_handle_from_payload(payload)


async def list_subagents() -> list[RLMSubagent]:
    """List direct RLM children of the current session (observable registry)."""
    payload = await host_request("rlm.list_subagents")
    entries = payload.get("subagents")
    if not isinstance(entries, list):
        raise RuntimeError("rlm.list_subagents returned an invalid subagents registry")
    return [_subagent_from_payload(entry) for entry in entries]


async def delete_subagent(target: str | RLMSubagent | RLMSpawnHandle) -> RLMSubagent:
    """Cancel a running child or remove a finished one.

    ``target`` may be an rlm_child_id, session_id, exact session_name, or a
    previously returned handle/dataclass. The child's session and kernel are
    disposed, its registry entry removed, and its session directory deleted.
    Returns the removed registry entry.
    """
    if isinstance(target, RLMSubagent):
        key: Any = target.rlm_child_id
    elif isinstance(target, RLMSpawnHandle):
        key = target.rlm_child_id
    else:
        key = target
    if not isinstance(key, str) or not key:
        raise TypeError(f"target must be a non-empty str or RLM subagent, got {type(target).__name__}")
    payload = await host_request("rlm.delete_subagent", {"target": key})
    return _subagent_from_payload(payload.get("subagent"), operation="rlm.delete_subagent")


# ---- M4 (R4): find_models, ported from prime-agent prime-agent-runtime/src/rlm/__init__.py


@dataclass(frozen=True)
class RLMModel:
    provider: str
    id: str
    name: str
    selector: str

    def __getitem__(self, key: str) -> Any:
        return getattr(self, key)


def _model_from_payload(payload: Any) -> RLMModel:
    if not isinstance(payload, dict):
        raise RuntimeError("rlm.find_models returned an invalid model entry")
    provider = payload.get("provider")
    model_id = payload.get("id")
    name = payload.get("name")
    selector = payload.get("selector")
    if not all(isinstance(value, str) and value for value in (provider, model_id, name, selector)):
        raise RuntimeError("rlm.find_models returned an invalid model entry")
    return RLMModel(provider=provider, id=model_id, name=name, selector=selector)


async def find_models(query: str = "", limit: int = 8) -> list[RLMModel]:
    """Search a bounded list of models backed by active user credentials."""
    if not isinstance(query, str):
        raise TypeError(f"query must be str, got {type(query).__name__}")
    if not isinstance(limit, int):
        raise TypeError(f"limit must be int, got {type(limit).__name__}")
    payload = await host_request("rlm.find_models", {"query": query, "limit": limit})
    models = payload.get("models")
    if not isinstance(models, list):
        raise RuntimeError("rlm.find_models returned an invalid models list")
    return [_model_from_payload(model) for model in models]


async def snapshot_save() -> dict[str, Any]:
    """Schedule an explicit kernel-state snapshot to run right after this cell.

    Returns immediately with {scheduled, path} — the snapshot runs as an
    internal kernel cell once the current cell finishes, so it never deadlocks
    the cell that requested it.
    """
    return await host_request("rlm.snapshot_save")


async def snapshot_restore() -> dict[str, Any]:
    """Schedule a restore of the on-disk snapshot after this cell finishes.

    Same scheduled semantics as snapshot_save(); restored names are visible in
    subsequent cells.
    """
    return await host_request("rlm.snapshot_restore")


class _RLMCallable:
    async def run(self, prompt: str, **kwargs: Any) -> RLMSpawnHandle:
        return await run(prompt, **kwargs)

    async def find_models(self, query: str = "", limit: int = 8) -> list[RLMModel]:
        return await find_models(query, limit)

    async def list_subagents(self) -> list[RLMSubagent]:
        return await list_subagents()

    async def delete_subagent(self, target: str | RLMSubagent | RLMSpawnHandle) -> RLMSubagent:
        return await delete_subagent(target)

    async def snapshot_save(self) -> dict[str, Any]:
        return await snapshot_save()

    async def snapshot_restore(self) -> dict[str, Any]:
        return await snapshot_restore()

    async def __call__(self, prompt: str, **kwargs: Any) -> RLMSpawnHandle:
        return await run(prompt, **kwargs)


rlm = _RLMCallable()


class _CallableModule(types.ModuleType):
    async def __call__(self, prompt: str, **kwargs: Any) -> RLMSpawnHandle:
        return await run(prompt, **kwargs)


sys.modules[__name__].__class__ = _CallableModule

# M4: PA skills import `from rlm import host_request` — alias this module under
# the `rlm` name so bundled PA skills load unchanged in our kernel.
sys.modules.setdefault("rlm", sys.modules[__name__])

__all__ = [
    "McpIntegration",
    "McpToolError",
    "NotEnabled",
    "RLMModel",
    "RLMSpawnHandle",
    "RLMSubagent",
    "delete_subagent",
    "find_models",
    "host_request",
    "list_subagents",
    "rlm",
    "run",
    "snapshot_restore",
    "snapshot_save",
]

# Lazily re-export the MCP base class (ported from prime-agent mcp_base.py).
# Kept lazy so `import rlm` never requires the optional `mcp` SDK — only
# integration packages (linear, notion) that subclass it do.
_LAZY_MCP = {"McpIntegration", "McpToolError", "NotEnabled"}


def __getattr__(name: str) -> Any:  # module-level lazy attr hook
    if name in _LAZY_MCP:
        from . import mcp_base

        return getattr(mcp_base, name)
    raise AttributeError(f"module {__name__!r} has no attribute {name!r}")
