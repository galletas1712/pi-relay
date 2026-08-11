"""Base class for MCP-client integrations exposed in the RLM kernel.

An integration is a Python skill package that subclasses :class:`McpIntegration`,
declares the MCP ``server`` it targets, and is imported in the kernel like any
other skill. Tools are auto-discovered from the server and bound as async
methods, so the agent writes ordinary Python:

    import linear
    issues = await linear.list_issues(team="Engineering")

Credentials live in the host's ``auth.json`` (single store, survives kernel
rebuilds). This module reads that file directly for the common case; on token
expiry it asks the host to refresh via ``rlm.host_request("mcp.refresh", ...)``
and re-reads. Interactive login runs host-side, never here.
"""
# Ported from prime-agent prime-agent-runtime/src/rlm/mcp_base.py (M4, verbatim below this line).


from __future__ import annotations

import asyncio
import json
import os
import time
from contextlib import AsyncExitStack
from pathlib import Path
from typing import Any

from . import host_request

__all__ = ["McpIntegration", "McpToolError", "NotEnabled"]

# Stored access tokens are treated as expired this many seconds early so a token
# never dies mid-request. Mirrors the host's refresh buffer.
_EXPIRY_SKEW_SECONDS = 30


class NotEnabled(RuntimeError):
    """Raised when an integration has no usable credentials.

    The integration is installed but not logged in. The message tells the agent
    how to enable it so it can relay that to the user rather than retrying.
    """

    def __init__(self, server: str):
        self.server = server
        super().__init__(
            f"The '{server}' integration is not enabled: no credentials found. "
            f"Tell the user to run `/mcp login {server}` in Prime Agent to connect it. "
            f"Do not ask them to set environment variables."
        )


class McpToolError(RuntimeError):
    """Raised when an MCP tool call returns a result flagged as an error."""


def _agent_dir() -> Path:
    """Resolve the Prime Agent config dir the same way the rest of the runtime does."""
    raw = (
        os.environ.get("PRIME_AGENT_CODING_AGENT_DIR")
        or os.environ.get("PI_CODING_AGENT_DIR")
        or str(Path.home() / ".prime" / "agent")
    )
    # resolve() so a relative env override reads auth.json from the right place,
    # not relative to the kernel's cwd.
    return Path(raw).expanduser().resolve()


def _read_auth(provider: str) -> dict[str, Any] | None:
    """Read one credential entry from auth.json. Returns None if absent/unreadable."""
    try:
        data = json.loads((_agent_dir() / "auth.json").read_text())
    except (OSError, ValueError):
        return None
    if not isinstance(data, dict):
        return None
    cred = data.get(provider)
    return cred if isinstance(cred, dict) else None


def _resolve_config_value(value: str) -> str:
    """Resolve a stored api_key value the way the host does.

    A value may be a literal, an env-var name, or a `!command` indirection. The
    command form can't run safely in the kernel (the host injects those resolved),
    so skip it; otherwise treat the value as an env-var name if set, else literal.
    """
    value = value.strip()
    if not value or value.startswith("!"):
        return ""
    return (os.environ.get(value) or value).strip()


def _resolve_streamable_http():
    """Return an SDK streamable-HTTP transport callable.

    SDK versions vary: some expose ``streamablehttp_client(url, headers=...)``,
    others ``streamable_http_client(url, *, http_client=...)``, and some expose
    both with *different* signatures. Imported lazily so importing an integration
    package never hard-fails when ``mcp`` is absent.
    """
    from mcp.client import streamable_http as mod  # noqa: PLC0415

    for name in ("streamablehttp_client", "streamable_http_client"):
        fn = getattr(mod, name, None)
        if fn is not None:
            return fn
    raise ImportError(
        "the installed `mcp` SDK exposes no streamable-HTTP client; upgrade `mcp`"
    )


class McpIntegration:
    """Subclass and set :attr:`server` (and :attr:`url` for remote servers).

    Tools are discovered on first use and bound as async methods via
    ``__getattr__``; ``await self.call_tool(name, args)`` is the explicit escape
    hatch and the hook for hand-written typed wrappers.
    """

    #: Credential / config key for this integration (matches the auth.json entry
    #: ``mcp:<server>`` and the mcpServers settings key).
    server: str = ""

    #: Remote MCP endpoint. Required unless a subclass overrides ``_open_streams``.
    url: str | None = None

    #: Optional env var holding a static bearer token (used instead of auth.json OAuth).
    bearer_token_env: str | None = None

    def __init__(self) -> None:
        if not self.server:
            raise ValueError(f"{type(self).__name__} must set a non-empty `server`")
        self._tools: dict[str, Any] | None = None
        self._lock = asyncio.Lock()

    # -- credentials --------------------------------------------------------

    @property
    def _provider_id(self) -> str:
        return f"mcp:{self.server}"

    def _token(self) -> str | None:
        """Current usable bearer token, or None if missing/expired (needs refresh).

        A static bearer-token env var wins (matches the host's `isAuthed` check);
        otherwise read auth.json. OAuth tokens are only returned while still fresh.
        """
        if self.bearer_token_env:
            env_token = os.environ.get(self.bearer_token_env, "").strip()
            if env_token:
                return env_token
        cred = _read_auth(self._provider_id)
        if cred is None:
            return None
        if cred.get("type") == "api_key":
            return _resolve_config_value(str(cred.get("key") or "")) or None
        # OAuth credential: {access, refresh, expires(ms)}.
        access = str(cred.get("access") or "")
        expires = cred.get("expires")
        fresh = isinstance(expires, (int, float)) and (
            time.time() * 1000 < expires - _EXPIRY_SKEW_SECONDS * 1000
        )
        if access and fresh:
            return access
        return None  # signal: needs refresh

    async def _resolve_token(self) -> str:
        token = self._token()
        if token:
            return token
        # Expired or missing-access: ask the host to refresh, then re-validate via
        # _token() (which re-checks expiry) rather than trusting any access value.
        if _read_auth(self._provider_id) is not None:
            refresh_error: Exception | None = None
            try:
                await host_request("mcp.refresh", {"server": self.server})
            except RuntimeError as exc:
                refresh_error = exc
            token = self._token()
            if token:
                return token
            # A refresh that failed (vs. genuinely-absent creds) is a recoverable
            # error; don't mislabel it as "not enabled / re-login".
            if refresh_error is not None:
                raise RuntimeError(
                    f"Failed to refresh credentials for '{self.server}': {refresh_error}"
                ) from refresh_error
        raise NotEnabled(self.server)

    # -- connection ---------------------------------------------------------

    async def _resolve_config(self) -> tuple[str | None, dict[str, str]]:
        """Host-resolved (url, extra_headers), honoring a user's mcpServers override.
        Falls back to the class ``url`` and no extra headers on host error."""
        try:
            cfg = await host_request("mcp.config", {"server": self.server})
        except RuntimeError:
            cfg = {}
        url = cfg.get("url") if isinstance(cfg, dict) else None
        headers = cfg.get("headers") if isinstance(cfg, dict) else None
        if not (isinstance(url, str) and url):
            url = self.url
        extra = headers if isinstance(headers, dict) else {}
        return url, {str(k): str(v) for k, v in extra.items()}

    async def _open_session(self, stack: AsyncExitStack):
        """Open an initialized MCP ClientSession bound to ``stack``.

        Override for non-HTTP transports (e.g. stdio). The default connects over
        streamable HTTP with a Bearer token from auth.json. The URL comes from the
        host (mcpServers override) when available, else ``self.url``.
        """
        import inspect  # noqa: PLC0415

        from mcp import ClientSession  # noqa: PLC0415

        url, extra_headers = await self._resolve_config()
        if not url:
            raise ValueError(
                f"{type(self).__name__} must set `url` or override `_open_session`"
            )
        token = await self._resolve_token()
        transport = _resolve_streamable_http()
        # Extra configured headers first, Authorization last so it always wins.
        auth_header = {**extra_headers, "Authorization": f"Bearer {token}"}

        # SDK signatures vary: some take headers=, others only http_client=.
        params = inspect.signature(transport).parameters
        if "headers" in params:
            cm = transport(url, headers=auth_header)
        elif "http_client" in params:
            import httpx  # noqa: PLC0415

            client = await stack.enter_async_context(httpx.AsyncClient(headers=auth_header))
            cm = transport(url, http_client=client)
        else:
            raise RuntimeError(
                f"unsupported mcp streamable-HTTP client signature: {tuple(params)}"
            )

        read, write, *_ = await stack.enter_async_context(cm)
        session = await stack.enter_async_context(ClientSession(read, write))
        await session.initialize()
        return session

    # -- tools --------------------------------------------------------------

    async def list_tools(self) -> list[dict[str, Any]]:
        """Return the server's tools as ``[{name, description, inputSchema}]``."""
        await self._ensure_tools()
        return [dict(t) for t in (self._tools or {}).values()]

    async def _ensure_tools(self) -> None:
        if self._tools is not None:
            return
        async with self._lock:
            if self._tools is not None:
                return
            async with AsyncExitStack() as stack:
                session = await self._open_session(stack)
                resp = await session.list_tools()
                self._tools = {
                    t.name: {
                        "name": t.name,
                        "description": getattr(t, "description", "") or "",
                        "inputSchema": getattr(t, "inputSchema", None) or {},
                    }
                    for t in resp.tools
                }

    async def call_tool(self, tool: str, arguments: dict[str, Any] | None = None) -> Any:
        """Call ``tool`` on the server and return its parsed result.

        Opens a fresh session per call: MCP sessions are not safe to hold across
        the kernel's snapshot/restore, and per-call connect keeps this robust to
        idle sessions and token rotation at modest latency cost.
        """
        async with AsyncExitStack() as stack:
            session = await self._open_session(stack)
            result = await session.call_tool(tool, arguments or {})
        return _parse_result(result)

    def __getattr__(self, name: str):
        # Only reached for names not found normally; bind as an async tool call.
        if name.startswith("_"):
            raise AttributeError(name)

        async def _call(**kwargs: Any) -> Any:
            await self._ensure_tools()
            if self._tools is not None and name not in self._tools:
                available = ", ".join(sorted(self._tools)) or "(none)"
                raise AttributeError(
                    f"'{self.server}' has no tool '{name}'. Available: {available}"
                )
            return await self.call_tool(name, kwargs)

        _call.__name__ = name
        _call.__qualname__ = f"{type(self).__name__}.{name}"
        if self._tools and name in self._tools:
            schema = self._tools[name].get("inputSchema") or {}
            desc = self._tools[name].get("description") or ""
            _call.__doc__ = f"{desc}\n\nArguments (JSON Schema):\n{json.dumps(schema, indent=2)}"
        return _call


def _parse_result(result: Any) -> Any:
    """Normalize a CallToolResult into plain Python (structured output preferred).

    Raises McpToolError when the server flags the result as an error, so a failed
    tool call doesn't look like a successful one to the caller.
    """
    texts: list[str] = []
    for block in getattr(result, "content", None) or []:
        text = getattr(block, "text", None)
        if text is not None:
            texts.append(text)
    if getattr(result, "isError", False):
        raise McpToolError("\n".join(texts) or "MCP tool returned an error")

    structured = getattr(result, "structuredContent", None)
    if structured is not None:  # falsy-but-valid payloads ({} / []) are real results
        return structured
    if texts:
        return "\n".join(texts)

    # Non-text content (images, embedded resources): return them as plain dicts
    # rather than the opaque SDK object so callers get usable data.
    blocks = getattr(result, "content", None) or []
    if blocks:
        return [b.model_dump(mode="json") if hasattr(b, "model_dump") else b for b in blocks]
    return result


# ---------------------------------------------------------------------------
# M8: pi-relay catalog-driven integration.
#
# The pi-relay bridge writes a per-session MCP catalog (selected servers,
# transports, auth kinds, config fingerprints) plus generated skills, and hands
# both to the kernel through the environment:
#
#   PI_RELAY_MCP_CATALOG       — <stateRoot>/sessions/<id>/mcp/catalog.json
#   PI_RELAY_MCP_CREDENTIALS   — <stateRoot>/mcp-oauth-credentials.json (0600)
#
# OAuth tokens live ONLY in that store (never auth.json, never PG, never the
# workspace). The file has two writers — the bridge and any number of kernels
# refreshing — so every read-modify-write takes the cross-process lock
# (<path>.lock dir, 60s stale break) and writes atomically (temp + rename,
# mode 0600), mirroring packages/bridge/src/mcp/credentials.ts.
# ---------------------------------------------------------------------------

import hashlib
import re
import stat
import tempfile

_CATALOG_ENV = "PI_RELAY_MCP_CATALOG"
_CREDENTIALS_ENV = "PI_RELAY_MCP_CREDENTIALS"
_CREDENTIAL_FILE_VERSION = 1
_LOCK_STALE_SECONDS = 60
_LOCK_TIMEOUT_SECONDS = 10


def _catalog_path() -> Path | None:
    raw = os.environ.get(_CATALOG_ENV, "").strip()
    return Path(raw) if raw else None


def _load_catalog() -> dict[str, Any]:
    path = _catalog_path()
    if path is None:
        raise RuntimeError(
            f"{_CATALOG_ENV} is not set: this kernel was not spawned with an MCP "
            "session catalog (create the session with an mcpSelection)."
        )
    try:
        data = json.loads(path.read_text())
    except (OSError, ValueError) as exc:
        raise RuntimeError(f"failed to read MCP catalog {path}: {exc}") from exc
    if not isinstance(data, dict) or not isinstance(data.get("servers"), list):
        raise RuntimeError(f"MCP catalog {path} is malformed")
    return data


def _credentials_path() -> Path:
    raw = os.environ.get(_CREDENTIALS_ENV, "").strip()
    if not raw:
        raise RuntimeError(
            f"{_CREDENTIALS_ENV} is not set: cannot reach the OAuth credential store"
        )
    return Path(raw)


def _credential_key(server_id: str, server_url: str) -> str:
    """credentialKey: sha256 of compact {"headers":{},"type":"http","url":...}
    (serde_json BTreeMap key order), first 16 hex chars — must match the bridge."""
    payload = '{"headers":{},"type":"http","url":' + json.dumps(server_url, separators=(",", ":")) + "}"
    return f"{server_id}|{hashlib.sha256(payload.encode()).hexdigest()[:16]}"


def _read_credential_file(path: Path) -> dict[str, Any]:
    try:
        raw = path.read_text()
    except FileNotFoundError:
        return {"version": _CREDENTIAL_FILE_VERSION, "credentials": {}}
    except OSError as exc:
        raise RuntimeError(f"mcp_oauth_credential_store_failed: io ({exc})") from exc
    if not raw.strip():
        raise RuntimeError("mcp_oauth_credential_store_failed: empty")
    try:
        data = json.loads(raw)
    except ValueError as exc:
        raise RuntimeError("mcp_oauth_credential_store_failed: corrupt") from exc
    if data.get("version") != _CREDENTIAL_FILE_VERSION:
        raise RuntimeError("mcp_oauth_credential_store_failed: unsupported_version")
    creds = data.get("credentials")
    if not isinstance(creds, dict):
        raise RuntimeError("mcp_oauth_credential_store_failed: corrupt")
    return data


class _FileLock:
    """mkdir-based cross-process lock with a stale break; mirrors credentials.ts."""

    def __init__(self, target: Path):
        self._dir = target.with_name(target.name + ".lock")

    def __enter__(self):
        deadline = time.monotonic() + _LOCK_TIMEOUT_SECONDS
        while True:
            try:
                os.mkdir(self._dir, 0o700)
                return self
            except FileExistsError:
                try:
                    st = self._dir.stat()
                    if time.time() - st.st_mtime > _LOCK_STALE_SECONDS:
                        os.rmdir(self._dir)
                        continue
                except OSError:
                    pass
                if time.monotonic() > deadline:
                    raise RuntimeError("mcp_oauth_credential_store_failed: lock_timeout")
                time.sleep(0.025)

    def __exit__(self, *exc):
        try:
            os.rmdir(self._dir)
        except OSError:
            pass
        return False


def _write_credential_file_atomic(path: Path, data: dict[str, Any]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    try:
        path.parent.chmod(0o700)
    except OSError:
        pass
    fd, tmp = tempfile.mkstemp(prefix=".mcp-oauth-", dir=path.parent)
    try:
        with os.fdopen(fd, "w") as fh:
            os.fchmod(fh.fileno(), 0o600)
            fh.write(json.dumps(data))
            fh.flush()
            os.fsync(fh.fileno())
        os.replace(tmp, path)
        path.chmod(0o600)
    finally:
        try:
            os.unlink(tmp)
        except OSError:
            pass


def _sanitize_tool_name(name: str) -> str:
    return re.sub(r"[^A-Za-z0-9_]", "_", name)


class CatalogMcpIntegration(McpIntegration):
    """McpIntegration driven by the pi-relay session catalog + credential store.

    Use :meth:`for_server`; the catalog pins transport, auth kind, timeouts and
    the SELECTED tool set (calls to unselected tools are rejected client-side,
    matching the bridge's control-plane mcp.call gating).
    """

    def __init__(self, server: str, entry: dict[str, Any]):
        self.server = server
        self._entry = entry
        transport = entry.get("transport") or {}
        self._transport_type = transport.get("type")
        if self._transport_type == "streamable_http":
            self.url = transport.get("url")
        self._call_timeout_ms = int(entry.get("call_timeout_ms") or 30_000)
        self._selected_tools: list[str] | None = (
            list(entry["tools"]) if isinstance(entry.get("tools"), list) else None
        )
        self.bearer_token_env = (
            transport.get("auth", {}).get("env")
            if transport.get("auth", {}).get("kind") == "bearer_env"
            else None
        )
        super().__init__()

    async def _resolve_config(self) -> tuple[str | None, dict[str, str]]:
        """The pi-relay session catalog IS the config source — never ask a host
        (the pi-relay host has no mcp.config/mcp.refresh comm handlers)."""
        return (str(self.url) if self.url else None), {}

    @classmethod
    def for_server(cls, server: str) -> "CatalogMcpIntegration":
        catalog = _load_catalog()
        for entry in catalog["servers"]:
            if isinstance(entry, dict) and entry.get("server") == server:
                return cls(server, entry)
        available = ", ".join(
            str(e.get("server")) for e in catalog["servers"] if isinstance(e, dict)
        )
        raise NotEnabled(server) if not available else RuntimeError(
            f"MCP server '{server}' is not in this session's selection (have: {available})"
        )

    # -- credentials (credential store, not auth.json) ----------------------

    def _stored_credential(self) -> dict[str, Any] | None:
        data = _read_credential_file(_credentials_path())
        cred = data.get("credentials", {}).get(_credential_key(self.server, str(self.url)))
        return cred if isinstance(cred, dict) else None

    def _token(self) -> str | None:
        if self._transport_type != "streamable_http":
            return None
        transport = self._entry.get("transport") or {}
        auth = transport.get("auth") or {}
        kind = auth.get("kind", "none")
        if kind == "none":
            return ""  # no Authorization header
        if kind == "bearer_env":
            env_token = os.environ.get(str(auth.get("env") or ""), "").strip()
            return env_token or None
        # oauth: read the shared store
        try:
            cred = self._stored_credential()
        except RuntimeError:
            return None
        if cred is None:
            return None
        access = str(cred.get("access_token") or "")
        expires = cred.get("expires_at_millis")
        fresh = access and (
            not isinstance(expires, (int, float))
            or time.time() * 1000 < expires - _EXPIRY_SKEW_SECONDS * 1000
        )
        return access if fresh else None

    async def _refresh_oauth(self) -> None:
        """refresh_token grant against the discovered AS, under the store lock.

        Discovery mirrors the bridge: RFC 8414 (path-aware then root) then OIDC.
        The refresh itself re-reads the store INSIDE the lock — another kernel or
        the bridge may have rotated first, in which case we keep their tokens.
        """
        import httpx  # noqa: PLC0415

        cred_path = _credentials_path()
        with _FileLock(cred_path):
            cred = _read_credential_file(cred_path).get("credentials", {}).get(
                _credential_key(self.server, str(self.url))
            )
            if cred is None:
                raise NotEnabled(self.server)
            expires = cred.get("expires_at_millis")
            if isinstance(expires, (int, float)) and time.time() * 1000 < expires - _EXPIRY_SKEW_SECONDS * 1000:
                return  # someone else refreshed while we waited for the lock
            refresh_token = str(cred.get("refresh_token") or "")
            if not refresh_token:
                raise RuntimeError(
                    f"OAuth credential for '{self.server}' expired with no refresh token; re-login required"
                )
            base = str(self.url)
            parsed = httpx.URL(base)
            origin = f"{parsed.scheme}://{parsed.host}" + (f":{parsed.port}" if parsed.port else "")
            path = parsed.path.rstrip("/")
            candidates = [
                f"{origin}/.well-known/oauth-authorization-server{path}",
                f"{origin}/.well-known/oauth-authorization-server",
                f"{origin}/.well-known/openid-configuration{path}",
                f"{origin}/.well-known/openid-configuration",
            ]
            metadata: dict[str, Any] | None = None
            async with httpx.AsyncClient(timeout=15) as client:
                for candidate in candidates:
                    try:
                        resp = await client.get(candidate)
                    except httpx.HTTPError:
                        continue
                    if resp.status_code == 200:
                        try:
                            metadata = resp.json()
                        except ValueError:
                            continue
                        break
                if not metadata or not metadata.get("token_endpoint"):
                    raise RuntimeError(f"mcp oauth discovery failed for {self.server}")
                resp = await client.post(
                    str(metadata["token_endpoint"]),
                    data={
                        "grant_type": "refresh_token",
                        "refresh_token": refresh_token,
                        "client_id": str(cred.get("client_id") or ""),
                    },
                )
                if resp.status_code != 200:
                    raise RuntimeError(
                        f"mcp oauth refresh failed for {self.server}: HTTP {resp.status_code}"
                    )
                tokens = resp.json()
            access = str(tokens.get("access_token") or "")
            if not access:
                raise RuntimeError(f"mcp oauth refresh returned no access token for {self.server}")
            updated = dict(cred)
            updated["access_token"] = access
            if tokens.get("refresh_token"):
                updated["refresh_token"] = str(tokens["refresh_token"])
            if isinstance(tokens.get("expires_in"), (int, float)):
                updated["expires_at_millis"] = int(time.time() * 1000 + tokens["expires_in"] * 1000)
            if tokens.get("scope"):
                updated["granted_scopes"] = [s for s in str(tokens["scope"]).split(" ") if s]
            data = _read_credential_file(cred_path)
            data.setdefault("credentials", {})[_credential_key(self.server, str(self.url))] = updated
            _write_credential_file_atomic(cred_path, data)

    async def _resolve_token(self) -> str:
        token = self._token()
        if token is not None:
            return token
        transport = self._entry.get("transport") or {}
        if (transport.get("auth") or {}).get("kind") == "oauth":
            await self._refresh_oauth()
            token = self._token()
            if token is not None:
                return token
            raise RuntimeError(f"Failed to refresh credentials for '{self.server}'")
        raise NotEnabled(self.server)

    # -- transport ----------------------------------------------------------

    async def _open_session(self, stack: AsyncExitStack):
        if self._transport_type == "stdio":
            from mcp import ClientSession, StdioServerParameters  # noqa: PLC0415
            from mcp.client.stdio import stdio_client  # noqa: PLC0415

            t = self._entry.get("transport") or {}
            env = dict(t.get("env") or {})
            for name in t.get("inherit_env") or []:
                if name in os.environ:
                    env[name] = os.environ[name]
            cm = stdio_client(
                StdioServerParameters(
                    command=str(t.get("command") or ""),
                    args=[str(a) for a in t.get("args") or []],
                    cwd=t.get("cwd"),
                    env=env or None,
                )
            )
            read, write = await stack.enter_async_context(cm)
            session = await stack.enter_async_context(ClientSession(read, write))
            await session.initialize()
            return session
        # streamable HTTP; unauthenticated endpoints take no Authorization header
        if (self._entry.get("transport") or {}).get("auth", {}).get("kind", "none") == "none":
            import inspect  # noqa: PLC0415

            from mcp import ClientSession  # noqa: PLC0415

            transport = _resolve_streamable_http()
            params = inspect.signature(transport).parameters
            if "headers" in params:
                cm = transport(str(self.url), headers={})
            else:
                cm = transport(str(self.url))
            read, write, *_ = await stack.enter_async_context(cm)
            session = await stack.enter_async_context(ClientSession(read, write))
            await session.initialize()
            return session
        return await super()._open_session(stack)

    # -- selection gating + sanitized aliases --------------------------------

    async def _ensure_tools(self) -> None:
        await super()._ensure_tools()
        if self._selected_tools is not None and self._tools is not None:
            self._tools = {k: v for k, v in self._tools.items() if k in self._selected_tools}

    async def call_tool(self, tool: str, arguments: dict[str, Any] | None = None) -> Any:
        if self._selected_tools is not None and tool not in self._selected_tools:
            selected = ", ".join(sorted(self._selected_tools)) or "(none)"
            raise RuntimeError(
                f"MCP tool '{tool}' is not in this session's selection for '{self.server}' "
                f"(selected: {selected})"
            )
        return await super().call_tool(tool, arguments)

    def __getattr__(self, name: str):
        # map sanitized attribute names back to raw tool names (mock.echo → mock_echo)
        if not name.startswith("_") and self._tools:
            for raw in self._tools:
                if _sanitize_tool_name(raw) == name:
                    name = raw
                    break
        return super().__getattr__(name)
