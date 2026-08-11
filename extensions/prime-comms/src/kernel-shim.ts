// Kernel-side python shim for prime-comms (M2).
//
// PROVENANCE: ported from prime-agent's python skills
//   packages/coding-agent/skills/agent-message/src/agent_message/__init__.py
//   packages/coding-agent/skills/agent-observe/src/agent_observe/__init__.py
// adapted to call prime_rlm_runtime.host_request (the M1 kernel bridge)
// instead of prime-agent's in-kernel `rlm` object. Installed into sys.modules
// by the prime-rlm bootstrap seam so `import agent_message` and a pre-bound
// `agent_message` global both work in kernel cells.

export const COMMS_KERNEL_PYTHON = `
import sys as _pc_sys
import types as _pc_types

from prime_rlm_runtime import host_request as _pc_host_request

_PC_MESSAGE_DISPLAY_MIME = "application/vnd.prime-agent.agent-message+json"


def _pc_emit_sent_message(receipt, receiver_role=None):
    try:
        from IPython.display import display as _pc_display
    except Exception:
        return
    try:
        if not isinstance(receipt, dict):
            return
        status = receipt.get("deliveryStatus")
        if status == "failed":
            return
        label = "Agent message queued" if status == "queued" else "Agent message sent"
        # Normalize to the kernel's sent-agent-message display contract
        # (prime-agent shape): id/message/deliveryStatus("delivered"|"queued")/
        # receiverRole/target{activeSessionId,sessionId,sessionName?}.
        target = receipt.get("target")
        target = target if isinstance(target, dict) else {}
        display_target = {
            "activeSessionId": receipt.get("source"),
            "sessionId": target.get("sessionId"),
        }
        if target.get("name"):
            display_target["sessionName"] = target.get("name")
        display_receipt = {
            "id": receipt.get("id"),
            "message": receipt.get("message"),
            "deliveryStatus": "queued" if status == "queued" else "delivered",
            "target": display_target,
        }
        if receiver_role in ("parent", "sibling", "child"):
            display_receipt["receiverRole"] = receiver_role
        _pc_display(
            {_PC_MESSAGE_DISPLAY_MIME: display_receipt, "text/plain": label},
            raw=True,
        )
    except Exception:
        pass


# --------------------------------------------------------------------------
# agent_message module
# --------------------------------------------------------------------------

agent_message = _pc_types.ModuleType("agent_message")
agent_message.__doc__ = """pi-relay session-to-session messaging (M2 port of prime-agent's agent_message skill)."""


async def _pc_am_list_agents():
    """List this agent's parent, siblings, and children, including inactive family."""
    return await _pc_host_request("agent_message.list_agents")


async def _pc_am_send(
    message,
    broadcast_message=None,
    *,
    receiver_role=None,
    receiver_name=None,
):
    """Send one direct role-addressed message or broadcast to ''all''."""
    roles = ("parent", "sibling", "child")
    if broadcast_message is not None:
        if message != "all":
            raise TypeError(
                "positional agent_message.send targets are not supported; "
                "use receiver_role and receiver_name"
            )
        if receiver_role is not None or receiver_name is not None:
            raise TypeError("broadcast cannot be combined with receiver_role/receiver_name")
        payload = {"target": "all", "message": broadcast_message}
    else:
        if receiver_role not in roles:
            raise ValueError('receiver_role must be "parent", "sibling", or "child"')
        if not isinstance(message, str):
            raise TypeError(f"message must be str, got {type(message).__name__}")
        if receiver_role == "parent":
            if receiver_name is not None:
                raise ValueError("receiver_name must be omitted for parent messages")
        elif not isinstance(receiver_name, str) or not receiver_name.strip():
            raise ValueError("receiver_name is required for sibling and child messages")
        payload = {
            "message": message,
            "receiver_role": receiver_role,
            "receiver_name": receiver_name,
        }
    receipt = await _pc_host_request("agent_message.send", payload)
    receipts = receipt.get("receipts") if isinstance(receipt, dict) else None
    if isinstance(receipts, list):
        for item in receipts:
            if isinstance(item, dict) and "deliveryStatus" in item:
                _pc_emit_sent_message(item)
    else:
        _pc_emit_sent_message(receipt, receiver_role)
    return receipt


agent_message.send = _pc_am_send
agent_message.list_agents = _pc_am_list_agents
_pc_sys.modules["agent_message"] = agent_message

# --------------------------------------------------------------------------
# agent_observe module
# --------------------------------------------------------------------------

agent_observe = _pc_types.ModuleType("agent_observe")
agent_observe.__doc__ = """Read-only pi-relay session observation (M2 port of prime-agent's agent_observe skill)."""


async def _pc_ao_list_agents():
    """List sessions visible to this agent (self + family)."""
    return await _pc_host_request("agent_observe.list")


async def _pc_ao_get_agent(target):
    """Read one session summary by role ("parent"), name, or "self"."""
    if not isinstance(target, str):
        raise TypeError(f"target must be str, got {type(target).__name__}")
    return await _pc_host_request("agent_observe.get", {"target": target})


async def _pc_ao_recent_messages(target, limit=8, max_chars=800):
    """Read bounded recent message previews from a visible session."""
    if not isinstance(target, str):
        raise TypeError(f"target must be str, got {type(target).__name__}")
    if not isinstance(limit, int):
        raise TypeError(f"limit must be int, got {type(limit).__name__}")
    if not isinstance(max_chars, int):
        raise TypeError(f"max_chars must be int, got {type(max_chars).__name__}")
    return await _pc_host_request(
        "agent_observe.recent",
        {"target": target, "limit": limit, "max_chars": max_chars},
    )


agent_observe.list_agents = _pc_ao_list_agents
agent_observe.get_agent = _pc_ao_get_agent
agent_observe.recent_messages = _pc_ao_recent_messages
_pc_sys.modules["agent_observe"] = agent_observe
`.trim();
