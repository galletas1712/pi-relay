# @pi-relay/web

React/Vite web client for pi-relay. The browser uses two same-origin
transports:

- `/ws` is the `pi-agentd` WebSocket RPC transport.
- `/api` is the read-only HTTP control surface served by the Rust `pi-web`
  process. Git inspection deliberately does not use `AgentApi` or WebSocket
  RPC.

## Develop

From the repository root, `infra/dev-web.sh` starts Postgres, `pi-agentd`,
`pi-web` on `127.0.0.1:8789`, and Vite on `127.0.0.1:8788`. Vite proxies
`/api` and `/healthz` to `PI_WEB_DEV_TARGET` (default
`http://127.0.0.1:8789`). It does not proxy `/ws`.

For frontend-only work:

```sh
PI_WEB_DEV_TARGET=http://127.0.0.1:8789 npm run dev --workspace @pi-relay/web
```

Run compatible `pi-agentd` and `pi-web` processes separately. The default
WebSocket URL is `ws://127.0.0.1:8787`; override it with
`VITE_PI_AGENT_WS`. Start `pi-agentd` first: it is the sole database migration
owner, while `pi-web` only verifies and reads the migrated session schema.

## Production-like local run

`infra/dev.sh` builds the Vite bundle and runs `pi-web`, which serves an
immutable startup snapshot of `packages/web/dist` with SPA fallback and the
same-origin API. Generated `dist` content is ignored and must not be committed.

When `TAILNET_HOST` is set, pair either workflow with `infra/serve.sh`.
Tailscale routes `/ws` directly to `pi-agentd` and all other paths to the web
port. No browser CORS permission is required.

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
