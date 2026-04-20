# Environment Variables

pi-relay reads a handful of `PI_*` environment variables for configuration and debugging, plus provider-specific variables for credentials and advanced routing. This page is the canonical reference.

The app name (`pi` here) is configurable via `piConfig.name` in `packages/coding-agent/package.json`. If a downstream fork renames the app to e.g. `tau`, the `PI_*` prefix is replaced with `TAU_*` for the one variable derived from the app name (`PI_CODING_AGENT_DIR`).

## Boolean convention

Most boolean flags accept `1`, `true`, or `yes` (case-insensitive) as "enabled" and treat anything else (including unset, `0`, `false`, empty) as "disabled". A few use strict `=== "1"` — those are called out in the table. `PI_CACHE_RETENTION` is value-typed (`none` / `long`) and uses strict string equality.

## User-facing

| Variable | Values | Default | Effect |
|----------|--------|---------|--------|
| `PI_CODING_AGENT_DIR` | absolute path | `~/.pi/agent` | Session storage and config directory. Renamed to `<APP_NAME>_CODING_AGENT_DIR` in forks. |
| `PI_PACKAGE_DIR` | absolute path | auto-detected | Override package directory (useful on Nix/Guix where store paths tokenize poorly). |
| `PI_SHARE_VIEWER_URL` | URL | `https://pi.dev/session/` | Base URL used by `/share` to build viewer links. |
| `PI_SKIP_VERSION_CHECK` | any value (truthy) | off | Skip the startup npm-registry version check. Also set implicitly by `PI_OFFLINE`. |
| `PI_OFFLINE` | `1` / `true` / `yes` | off | Disable all startup network operations (version check, install telemetry, package update checks). |
| `PI_TELEMETRY` | `1` / `true` / `yes` or `0` / `false` / `no` | settings default | Override install telemetry. See `settings.json` key `enableInstallTelemetry` for the persistent version. |
| `PI_AI_ANTIGRAVITY_VERSION` | version string | built-in default | Override the User-Agent version advertised to the Antigravity provider. |

## Prompt caching

| Variable | Values | Default | Effect |
|----------|--------|---------|--------|
| `PI_CACHE_RETENTION` | `none` | unset | Kill switch — disables all prompt-caching `cache_control` / `cachePoint` / `prompt_cache_key` stamps on every supported provider (Anthropic, Bedrock, OpenAI Responses, OpenAI Completions, Azure, Codex). |
| `PI_CACHE_RETENTION` | `long` | unset | Opt in to extended cache TTL where the provider supports it. Anthropic: 1h on `api.anthropic.com`. Bedrock: 1h cache point. OpenAI Responses / Azure / Codex: `prompt_cache_retention: "24h"`. Chat Completions has no extended-TTL wire field, so `long` is inert there. |
| `PI_SHOW_CACHE_STATS` | `=1` (strict) | off | Surface per-turn cache read/write tokens. TUI: adds a `Δ R:N W:M` column to the footer. Print mode: emits `[pi:cache] turn=N cacheRead=R cacheWrite=W input=I output=O` to stderr on each assistant `message_end`. |

## Debug / telemetry

| Variable | Values | Default | Effect |
|----------|--------|---------|--------|
| `PI_TUI_DEBUG` | `=1` (strict) | off | Emit debug logs about TUI events to stderr (e.g., input-queue drops during session switches). |
| `PI_TUI_WRITE_LOG` | file path | unset | Write raw TUI output stream to the given file, for reproducing rendering bugs. |
| `PI_DEBUG_REDRAW` | `=1` (strict) | off | Log TUI redraw decisions to `~/.pi/agent/pi-debug.log`. Useful when diagnosing flicker or layout glitches. |
| `PI_HARDWARE_CURSOR` | `=1` (strict) | settings default | Use the terminal's hardware cursor instead of the software-drawn one. Corresponds to settings key `showHardwareCursor`. |
| `PI_CLEAR_ON_SHRINK` | `=1` (strict) | off | Clear empty terminal rows when content shrinks instead of leaving stale bytes. Corresponds to settings key `terminal.clearOnShrink`. |
| `PI_TIMING` | `=1` (strict) | off | Print startup phase timing breakdown to stderr. |
| `PI_STARTUP_BENCHMARK` | `1` / `true` / `yes` | off | Run one-shot startup, measure time-to-ready, then exit. For benchmarking cold-start performance. |

## Provider-specific (advanced)

### Amazon Bedrock

These are for proxy / custom-endpoint scenarios. See [providers.md](./providers.md#amazon-bedrock) for full setup.

| Variable | Values | Effect |
|----------|--------|--------|
| `AWS_BEDROCK_SKIP_AUTH` | `=1` (strict) | Skip AWS SigV4 signing. Use when your Bedrock proxy does not require authentication. |
| `AWS_BEDROCK_FORCE_HTTP1` | `=1` (strict) | Force HTTP/1.1. Use when your Bedrock proxy does not support HTTP/2. |
| `AWS_BEDROCK_FORCE_CACHE` | `=1` (strict) | Force `cachePoint` emission for application inference profiles and other model IDs not in the built-in allowlist (e.g., Nova). |

### Provider API keys

See `pi --help` for the full list of provider credential environment variables (`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `AWS_PROFILE`, etc.) — not duplicated here.

## Related config files

| Path | Purpose |
|------|---------|
| `~/.pi/agent/settings.json` | Global user preferences. See [settings.md](./settings.md). |
| `.pi/settings.json` | Project-local override, deep-merged on top of the global. |
| `~/.pi/agent/auth.json` | OAuth tokens and API keys stored by `pi auth` flows. |
| `~/.pi/agent/models.json` | Custom provider / model definitions. See [custom-provider.md](./custom-provider.md). |
| `~/.pi/agent/sessions/<cwd-hash>/<session-id>/` | Per-session transcripts, worklogs, and tree state. |
| `~/.pi/agent/bin/` | Managed helper binaries (`fd`, `rg`). |
| `~/.pi/agent/pi-debug.log` | Debug log destination used by `PI_DEBUG_REDRAW`. |

`models.json` and `auth.json` entries can reference arbitrary environment variables as strings (e.g., `"apiKey": "MY_CUSTOM_KEY"` reads `process.env.MY_CUSTOM_KEY`) or run shell commands via `!<cmd>`. See [custom-provider.md](./custom-provider.md) for details.
