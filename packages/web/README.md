# @pi-relay/web

React/Vite web client for the pi-relay Rust agent daemon (`pi-agentd`).

## Develop

```sh
npm run dev:web   # from the repo root
```

Serves at `http://127.0.0.1:8788` and connects to `ws://127.0.0.1:8787` by
default; override the daemon URL with `VITE_PI_AGENT_WS`. Start the daemon
first - see [`../../rust/README.md`](../../rust/README.md).

## Documentation

- [`docs/web-ui.md`](docs/web-ui.md) - the client design: the data layer
  (TanStack Query for lists plus the normalized selected-session cache), the
  turn-card transcript, the queue pane, slash commands, and composer drafts.
- [`../../rust/docs/websocket-rpc.md`](../../rust/docs/websocket-rpc.md) - the
  RPC contract this client speaks.
- [`../../rust/docs/architecture.md`](../../rust/docs/architecture.md) - the
  overall system and crate map.

The New Session MCP picker includes generic OAuth login/logout when configured
by the daemon. OAuth transaction handles and authorization URLs are held only
in React memory, never browser storage. For a remote daemon, the login dialog
accepts the entire loopback callback URL copied from the browser.
