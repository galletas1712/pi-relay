# pi-ai Sidecar Migration Wiki

Incrementally replace pi-relay's hand-rolled Rust provider layer with
@oh-my-pi/pi-ai (the oh-my-pi fork), keeping the Rust daemon's
session/store/MCP/tools/transport intact.

## Documents

- [PLAN.md](PLAN.md) — Decision, gain/loss analysis, phased plan
- [DESIGN.md](DESIGN.md) — Architecture, seam analysis, translation map
- [CLEAN-SEAM.md](CLEAN-SEAM.md) — How ProviderKind is eliminated + marketplace investigation
- [SCOPE.md](SCOPE.md) — Implementation scope: 6 work packages, file inventory, gates
- [INDEX.md](INDEX.md) — This file
