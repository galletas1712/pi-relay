# π-relay

Personal-use π-relay agent runtime with a Rust control plane/runtime, durable
PostgreSQL session storage, MCP routes, and a React web UI.

## Quick links

- [Rust stack setup, services, credentials, and database behavior](rust/README.md)
- [Web UI documentation](packages/web/docs/web-ui.md)
- [Architecture and crate map](rust/docs/architecture.md)
- [Websocket RPC contract](rust/docs/websocket-rpc.md)
- [Local Docker/host development stack](infra/dev.sh)
- [Desktop Electron shell](packages/electron/README.md)

The Rust workspace is the product implementation. The web build is static and
backend-independent. Cloudflare Pages hosts `packages/web/dist`; each browser
keeps named control-server profiles and opens WebSockets directly to the
selected `pi-agentd`. The daemon accepts browser upgrades only from explicitly
configured canonical Origins; Tailnet ACLs or SSH remain the authorization
boundary.
`pi-runtime` runs on the control host so workspace tools and MCP servers retain
host filesystem/toolchain access.

Run `infra/dev.sh` for Postgres, control, and the host runtime. Run
`npm run dev:web` separately for loopback Vite development. Production browser
profiles use `wss://<control-node>.<tailnet>.ts.net:8443/`; see the
[web package README](packages/web/README.md) for Cloudflare Pages settings and
the static-host security model.

The optional desktop package is a macOS-only thin remote shell around the
deployed Cloudflare Pages frontend. It defaults to
`https://pi-relay.pages.dev` and can be pointed at another HTTP(S) deployment
with `PI_RELAY_WEB_URL`; it does not bundle the frontend or add another
backend. Frontend deployments are picked up on the next launch or after the
desktop window has been hidden for five minutes and returns to the foreground,
so an Electron reinstall is not normally needed. See
[desktop shell setup](packages/electron/README.md).

The repository requires Node.js 22.12 or newer because the Electron workspace
uses Electron 40.
