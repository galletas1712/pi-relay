# π-relay

Personal-use π-relay agent runtime with a Rust control plane/runtime, durable
PostgreSQL session storage, MCP routes, and a React web UI.

## Quick links

- [Rust stack setup, services, credentials, and database behavior](rust/README.md)
- [Web UI documentation](packages/web/docs/web-ui.md)
- [Architecture and crate map](rust/docs/architecture.md)
- [Websocket RPC contract](rust/docs/websocket-rpc.md)
- [Local Docker/host development stack](infra/dev.sh)

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
