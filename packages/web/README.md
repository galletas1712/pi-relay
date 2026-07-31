# @pi-relay/web

React/Vite web client for the pi-relay Rust agent daemon (`pi-agentd`).

## Develop

```sh
npm run dev:web   # from the repo root
```

Vite is only the React/TypeScript/Tailwind bundler and loopback HMR server. It
serves `http://127.0.0.1:8788`; when opened on loopback, a first-run browser
gets a `Local` profile for `ws://127.0.0.1:8787`. Run `infra/dev.sh` separately
for Postgres, `pi-agentd`, and the host `pi-runtime`.

## Production on Cloudflare Pages

Use Cloudflare Pages native Git integration with:

| Setting | Value |
| --- | --- |
| Root directory | repository root |
| Build command | `npm ci && npm run build --workspace @pi-relay/web` |
| Build output directory | `packages/web/dist` |

Set no backend URL or Vite endpoint environment variables. Cloudflare
Pages serves SPA fallback automatically because the build has no top-level
`404.html`. Configure one stable custom domain and allowlist that exact
serialized origin on every control host. Preview and generated Pages origins
are intentionally rejected.

The production profile URL is
`wss://<control-node>.<tailnet>.ts.net:8443/`. The
`packages/web/public/_headers` policy is copied into `dist`: scripts remain
self-hosted, framing and objects are prohibited, referrers are suppressed, and
direct WSS plus exact loopback WS destinations are allowed. Cloudflare's
default caching policy is used.

Exact Origin validation prevents unrelated webpages in honest browsers from
opening the control WebSocket. It is not CORS and does not authenticate
arbitrary clients: a non-browser client can forge `Origin`. Tailnet ACLs or SSH
authorize network access. A compromised allowlisted frontend, browser, or
device remains trusted. An HTTPS page cannot use an SSH-forwarded `ws://`
profile because of mixed-content rules; use the loopback Vite page for SSH
tunnels.

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
