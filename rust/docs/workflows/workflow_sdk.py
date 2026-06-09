"""Tiny pi-relay workflow SDK for editable workflow scripts.

This file is intentionally dependency-light and wraps only the compact daemon RPCs:
WorkSpawn, WorkAwait, WorkRead, WorkSend, WorkWrite via their websocket RPC names.
Copy this file beside a workflow script, edit freely, and rerun from deterministic ids.
"""

from __future__ import annotations

import itertools
import json
import os
import urllib.request
from dataclasses import dataclass
from typing import Any


@dataclass
class WorkflowClient:
    """Minimal JSON-RPC websocket client wrapper using Python's stdlib HTTP fallback.

    pi-agentd speaks websocket RPC. For template readability this class accepts a caller-provided
    rpc function. Live drivers can bind it to websocket-client, while agents can replace `rpc` with
    whatever session helper they already use.
    """

    source_session_id: str
    workflow_id: str
    rpc: Any

    def call(self, method: str, params: dict[str, Any]) -> dict[str, Any]:
        return self.rpc(method, params)

    def spawn_subagent(
        self,
        *,
        role: str,
        task: str,
        result_var: str,
        child_session_id: str,
        display_name: str | None = None,
        context_vars: list[str] | None = None,
    ) -> dict[str, Any]:
        context = self.context_block(context_vars or [])
        return self.call(
            "work.spawn",
            {
                "source_session_id": self.source_session_id,
                "role": role,
                "task": task,
                "initial_context": context,
                "workflow_id": self.workflow_id,
                "result_variable": result_var,
                "child_session_id": child_session_id,
                "display_name": display_name or role,
            },
        )

    def await_vars(self, names: list[str], timeout_ms: int = 120_000) -> dict[str, Any]:
        return self.call(
            "work.await",
            {
                "source_session_id": self.source_session_id,
                "workflow_id": self.workflow_id,
                "vars": names,
                "timeout_ms": timeout_ms,
            },
        )

    def await_sessions(self, session_ids: list[str], *, idle: bool = True, timeout_ms: int = 120_000) -> dict[str, Any]:
        return self.call(
            "work.await",
            {
                "source_session_id": self.source_session_id,
                "workflow_id": self.workflow_id,
                "sessions": session_ids,
                "idle": idle,
                "timeout_ms": timeout_ms,
            },
        )

    def read_var(self, name: str) -> Any:
        result = self.call(
            "work.read",
            {
                "source_session_id": self.source_session_id,
                "workflow_id": self.workflow_id,
                "view": "var",
                "var": name,
            },
        )
        return result.get("value_json") if result.get("value_json") is not None else result.get("value_text")

    def write_var(self, name: str, value: Any = None, text: str | None = None) -> dict[str, Any]:
        params: dict[str, Any] = {
            "source_session_id": self.source_session_id,
            "workflow_id": self.workflow_id,
            "var": name,
        }
        if text is not None:
            params["value_text"] = text
        else:
            params["value_json"] = value
        return self.call("work.write", params)

    def context_block(self, var_names: list[str]) -> str:
        parts = [f"Workflow id: {self.workflow_id}"]
        for name in var_names:
            try:
                parts.append(f"\n## {name}\n{json.dumps(self.read_var(name), indent=2)}")
            except Exception as exc:  # templates should be editable/resilient
                parts.append(f"\n## {name}\n<unavailable: {exc}>")
        parts.append(
            "\n## Reporting contract\n"
            "When finished, write your assigned result variable with WorkWrite."
        )
        return "\n".join(parts)
