# `agent-runtime` reference

`agent-runtime` (`pi-runtime`) is the host-side worker for a pi-relay
installation. The daemon owns durable sessions, provider policy, and websocket
RPC; the runtime owns host-local workspaces, local tools, skills, and MCP
connections.

## Process and control lifecycle

The runtime starts as a long-lived host process and connects to the configured
daemon control address using `agent-runtime-protocol`. The daemon can create
and manage runtime workspaces, invoke host-local tools, enumerate skills, and
proxy MCP operations. Runtime work is scoped to the host and is not durable
until the daemon persists the corresponding session or action state.

The runtime should be treated as independently restartable. A runtime restart
must not be used as a substitute for database recovery: the daemon reconciles
leased actions and session state when the control connection returns.

## Workspace and security boundary

Each runtime has a `workspace_root`. Managed workspaces, local tool execution,
skills, and runtime-owned MCP state are kept below that root unless a
configuration explicitly opts into another path. The runtime is a host-trust
boundary: it can read credentials and execute local tools available to the
runtime user, while the daemon remains the source of truth for session
authorization and durable state.

Run the runtime with the least-privileged account that can access its
workspace, required toolchain, and configured credential files. Do not mount
the runtime workspace or its credential directory into the control-plane
container unless that is an intentional deployment decision.

On Linux, workspace creation may rely on filesystem features such as Btrfs
reflinks when configured by the host deployment. A filesystem without the
required reflink behavior must be validated before production use; otherwise
workspace copies may be slower or fail during creation.

## Configuration

Runtime policy is read from the XDG configuration root:

```text
$XDG_CONFIG_HOME/pi-relay/runtime/config.toml
```

When `XDG_CONFIG_HOME` is unset, this is normally:

```text
$HOME/.config/pi-relay/runtime/config.toml
```

The configuration identifies the runtime, its display name, workspace root,
and control bind/connect settings. MCP policy is kept separately in the
runtime `mcp.toml` file when MCP is enabled.

The repository's local stack can start the runtime through `infra/dev.sh`.
For a manual run:

```sh
cargo run --manifest-path rust/Cargo.toml -p agent-runtime
```

See [`rust/README.md`](../README.md) for prerequisites and the local-stack
configuration examples.

## MCP and credentials

MCP clients run on the runtime host next to local tool execution. OAuth
credentials are stored beneath the runtime-owned configuration/workspace
boundary and are never intended to be returned through public RPCs. A failed
MCP route should remain isolated: unrelated stdio, bearer-token, and runtime
startup paths should continue when possible.

Keep credential files out of source control and ensure their permissions are
restricted to the runtime user. For the full MCP lifecycle and transport
contract, see [`plans/mcp-client.md`](plans/mcp-client.md).

## Control connection and recovery

The daemon sends framed control/runtime commands and receives structured
results. Commands may be retried after reconnect, so handlers must preserve
the request identity and avoid duplicating non-idempotent host work without the
corresponding action fence. The daemon's persisted action/revision protocol is
authoritative when a runtime reconnects with in-flight work.
