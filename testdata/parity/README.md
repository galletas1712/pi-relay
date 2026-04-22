# Parity fixture layout

Phase 0 only reserves the directory structure and baseline file names for replay
fixtures. Later milestones will teach the parity scripts how to execute these
fixtures against the extracted TypeScript cores and Rust shadow runtimes.

## Directory structure

```text
testdata/parity/
  README.md
  orchestrator/
    <fixture-name>/
      meta.json
      commands.ndjson
      events.ndjson
      expected-effects.ndjson
      expected-snapshot.json
      notes.md            # optional
  session/
    <fixture-name>/
      meta.json
      commands.ndjson
      events.ndjson
      expected-effects.ndjson
      expected-snapshot.json
      notes.md            # optional
```

## Required file

- `meta.json` — required for every fixture. Keep it small and human-readable.
  Suggested fields:
  - `id`
  - `title`
  - `surface` (`"orchestrator"` or `"session"`)
  - `source`
  - `description`
  - `notes`

## Recommended files

- `commands.ndjson` — normalized command stream sent into the core.
- `events.ndjson` — host observations fed back into the core during replay.
- `expected-effects.ndjson` — normalized effect stream emitted by the legacy
  implementation and used as the baseline for parity checks.
- `expected-snapshot.json` — normalized terminal snapshot for the scenario.
- `notes.md` — optional fixture-specific caveats, normalization notes, or links
  to a bug/regression that motivated the capture.

## Naming guidance

- Prefer durable scenario names such as `root-spawn-restore`,
  `background-tool-completion`, or `session-cancel-retry`.
- Keep one scenario per directory.
- Normalize timestamps, generated IDs, and other unstable values before storing
  expected outputs so diffs stay meaningful.

## Current scripts

- `node scripts/parity/orchestrator-replay.mjs`
- `node scripts/parity/session-replay.mjs`

In Phase 0 these scripts only discover fixtures and validate the scaffolded
layout. They intentionally do not execute replay logic yet.
