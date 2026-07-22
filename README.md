# pi-relay

Personal-use agent runtime with a Rust control plane/runtime, durable
PostgreSQL session storage, MCP routes, and a React web UI.

## Quick links

- [Rust stack setup, services, credentials, and database behavior](rust/README.md)
- [Web UI documentation](packages/web/docs/web-ui.md)
- [Architecture and crate map](rust/docs/architecture.md)
- [Websocket RPC contract](rust/docs/websocket-rpc.md)
- [Local Docker/host development stack](infra/dev.sh)

The Rust workspace is the product implementation. The web client connects to
`pi-agentd` over its websocket endpoint; `pi-runtime` runs on the host so
workspace tools and MCP servers retain host filesystem/toolchain access.
