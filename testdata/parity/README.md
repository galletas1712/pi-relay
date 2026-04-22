# Parity fixture layout

Phase 2 now wires real replay support for orchestrator fixtures while session
fixtures remain scaffolded for later milestones. Store normalized command,
effect, and snapshot captures here so TS-core replays and future Rust shadow
runs can diff against the same baseline.

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

## Required files

All fixtures should include:

- `meta.json` — required for every fixture. Keep it small and human-readable.
  Suggested fields:
  - `id`
  - `title`
  - `surface` (`"orchestrator"` or `"session"`)
  - `source`
  - `description`
  - `notes`

Replay-ready **orchestrator** fixtures currently also require:

- `commands.ndjson` — normalized command stream sent into the core.
- `expected-effects.ndjson` — normalized effect stream used as the parity
  baseline.
- `expected-snapshot.json` — normalized terminal snapshot for the scenario.

## Optional files

- `events.ndjson` — reserved host observations fed back into the core during
  replay. Phase 2 records them but does not replay them yet.
- `notes.md` — optional fixture-specific caveats, normalization notes, or links
  to a bug/regression that motivated the capture.

Session fixtures remain scaffold-only in Phase 2, so `commands.ndjson`,
`expected-effects.ndjson`, and `expected-snapshot.json` are still reserved for
later session-core milestones rather than enforced today.

## Naming guidance

- Prefer durable scenario names such as `root-spawn-restore`,
  `background-tool-completion`, or `session-cancel-retry`.
- Keep one scenario per directory.
- Normalize timestamps, generated IDs, and other unstable values before storing
  expected outputs so diffs stay meaningful.

## Current scripts

- `node scripts/parity/orchestrator-replay.mjs`
- `node scripts/parity/session-replay.mjs`

`orchestrator-replay.mjs` now loads fixtures, replays `commands.ndjson` against
`packages/orchestrator-core`, normalizes unstable fields, and reports diffs.
`session-replay.mjs` is still scaffold-only until the session-core milestones.

## Orchestrator normalization hints

Each orchestrator fixture can optionally include `meta.json.normalization` to
reduce diff noise without hiding real behavioral changes. Supported fields are:

- `pathFields` — exact property names whose values should be reduced to their
  basename (defaults to `sessionFile` and `worklogFile`)
- `timestampFields` — exact property names whose values should be tokenized as
  `T1`, `T2`, ... in deterministic sorted-key traversal order
- `stringReplacements` — exact string-to-string replacements applied before
  path/timestamp normalization

The sample fixture under `testdata/parity/orchestrator/root-child-lifecycle/`
shows a small replayable scenario using path normalization.
