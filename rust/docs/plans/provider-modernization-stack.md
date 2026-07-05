# Provider modernization stack boundaries

This document records the cumulative local restack from base `c84d36a` to the
tested production behavior at `84c7db0`. Each branch is independently
buildable and contains final-state hunks for its boundary rather than temporary
development phases. Later branches build on earlier branches in the order
listed here.

## 1. `restack/anthropic-provider-surface`

Modernizes the Anthropic-facing provider surface:

- current Claude model metadata, authenticated Models API discovery, bounded
  caching, and the conservative adapter fallback;
- current hosted web search/fetch schemas and transport headers;
- Fable retention warning and explicit UI selection;
- adapter-owned output ceilings; and
- the provider-neutral model metadata shape required by scheduling.

This boundary does not contain provider-native Anthropic compaction, durable
continuation, Codex discovery, public Ultra configuration, duplicate auth
coordination, or daemon-owned provider/model policy.

## 2. `restack/provider-stream-replay-correctness`

Adds the provider-neutral lifecycle and replay contract plus strict ordinary
OpenAI and Anthropic transport behavior:

- immutable provider-tagged replay with no generic name rewriting or dropping;
- semantic terminal requirements and staged/atomic output reconciliation;
- refusal, incomplete, maximum-output, and usage handling;
- strict Anthropic content-block sequencing; and
- strict canonical parsing for the pre-existing OpenAI remote compaction
  endpoint.

This boundary does not add the Codex catalog, Anthropic native compaction, a
daemon native-only cutover, or permissive compact parsing.

## 3. `restack/codex-model-capabilities`

Adds authenticated account-scoped private Codex model discovery and request
shaping:

- a bounded shared in-memory catalog cache;
- exact slug and public effort validation;
- current/default context-window recommendations;
- catalog-driven parallel-tool shaping;
- a seeded offline-safe web picker that offers catalog-proven `max` for Sol,
  Terra, and Luna without exposing catalog-only `ultra`;
- the existing single auth-refresh boundary; and
- removal of static OpenAI model policy from the daemon.

`max` is the highest public wire effort. Catalog-only `ultra` and future
strings remain bounded metadata and are never public configuration. There is
no duplicate auth coordinator, public Ultra phase, stale/static/public/disk
fallback, proactive agent mode, or native-compaction activation.
