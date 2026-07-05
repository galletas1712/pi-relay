# Rust Rewrite Worklog

## 2026-07-02

### Anthropic Model and Hosted-Tool Refresh

- Added Claude Sonnet 5 as the normal Claude UI option (`high` effort), retained
  Opus 4.8, and added Fable 5 as an explicit opt-in whose label/tooltip and docs
  call out Anthropic's required 30-day retention and lack of Zero Data
  Retention.
- Refreshed Messages request shaping from first-party Anthropic documentation:
  Sonnet 5/Fable 5 rely on their default/always-on adaptive thinking and omit
  manual thinking, while Opus 4.8 requests adaptive thinking explicitly. All
  support `low…max` through `output_config.effort`. Removed retired GA feature
  beta values while preserving `anthropic-version: 2023-06-01` and the existing
  Claude Code identity beta/header envelope on Messages requests; Models API
  discovery sends no undocumented beta.
- Integrated `GET /v1/models/{id}` with a shared in-memory cache (64 settled
  ids; six-hour success TTL and one-minute refresh-failure backoff). Per-model
  refreshes are single-flight without holding the cache lock across network
  I/O. In-flight records are never evicted and may temporarily exceed the
  settled-entry bound under concurrent capacity pressure; completion trims the
  cache back to 64. A failed refresh retains stale last-known-good metadata;
  only a cold lookup failure creates a negative entry. Discovered input/output
  limits and thinking/effort capabilities shape provider requests and
  proactive compaction. Static 1M-input/128K-output metadata for Sonnet 5 and
  Fable 5 keeps known models safe during discovery failure; unknown models
  retain a conservative 64K output ceiling and no assumed context window.
- Treats Fable's HTTP-200 `stop_reason: "refusal"` as a terminal model failure,
  not a completed assistant turn. Any partial streamed assistant content and
  provider replay are discarded, while `stop_details` category/explanation are
  retained in the surfaced error and action result. Refusals do not trigger an
  automatic retry or fallback model.
- Kept the ordinary Anthropic default output request at
  `min(64K, model ceiling)` rather than automatically reserving each model's
  full 128K. Explicit session limits are clamped to the discovered/static
  ceiling.
- Upgraded the Anthropic hosted web sidecar to `web_search_20260318` and
  `web_fetch_20260318`, direct callers only, with the documented
  `response_inclusion: "excluded"` shape and no obsolete web-fetch beta.
- Kept discovery at the daemon/provider seam instead of adding UI RPC or
  database storage: the UI's known options are deterministic offline fallbacks,
  provider handles share transient metadata across session reconstruction, and
  the compaction gate consults the same cached metadata.
- Verified with
  `cargo test --manifest-path rust/Cargo.toml -p agent-provider -p agent-daemon`
  (89 provider and 157 daemon tests), `cargo clippy --manifest-path
  rust/Cargo.toml -p agent-provider -p agent-daemon --all-targets` (strict
  `-D warnings` stops only on the pre-existing
  `agent-store::switch_active_leaf` argument-count warning),
  `npm run test --workspace @pi-relay/web` (191 tests), and
  `npm run build --workspace @pi-relay/web`.

## 2026-05-27

### Per-Session Workspace Subset And Git Branch Override

- Added an optional `workspaces` array to `session.start` so a project session can
  materialize a subset of the project's workspaces (keeping unrelated workspace
  dirs, their `AGENTS.md`, and skills out of the session `outer_cwd` and prompt)
  and can override the git branch each selected workspace starts from.
- Introduced `workspaces::selection` (`WorkspaceSelection` / `RequestedWorkspace` /
  `SelectedWorkspace`) to validate the request against the project: rejects empty
  selections, unknown dirs, duplicates, and branch overrides on local-folder
  workspaces; preserves project-declared order. Extracted from `workspaces/mod.rs`
  to keep that module focused (479 -> 376 LoC).
- `WorkspaceManager::materialize_session` now reconciles managed bases against the
  project's full set (so a subset never deletes skipped workspaces' bases) but only
  instantiates the selected subset. Git branch overrides fetch the requested branch
  into the session copy via `fetch_session_branch_head` after instantiation; the
  shared per-project base stays on the project's configured branch.
- Web: collapsible `WorkspaceScopePicker` above the composer for new project
  sessions, defaulting to all-included/default-branch (sends no `workspaces`, so
  daemon behavior is unchanged). Per-project choices persist in `localStorage`
  (`workspaceScope.ts`); stale entries for removed workspaces are dropped on
  re-derive.
- Verified with `cargo test -p agent-daemon` (45 tests, including 3 selection unit
  tests and a git branch-override materialization test) and the web suite via
  vitest (125 tests, including 5 new `workspaceScope` tests). Updated
  `docs/websocket-rpc.md` and `packages/web/docs/web-ui.md`.

## 2026-05-26

### Session Sync Redesign And Queued Follow-Up Mutations

- Added per-session revision counters and canonical queue projections so
  frontend/daemon/Postgres views converge by replacing stale state instead of
  applying inferred queue patches.
- Serialized short session-owned Postgres mutations with a row lock on the
  target `sessions` row only. Provider/tool/compaction I/O remains outside DB
  locks.
- Reworked new queued-input consumption to peek rows and fence the final
  transcript commit by row version plus canonical-next validation, avoiding new
  `queued -> consuming` leases while preserving legacy consuming-row recovery.
- Added Rust daemon/store RPC support for queued follow-up edit, cancel, and
  full-list reorder. Steering messages stay at the top, keep steer/promote
  order, and are not editable/reorderable through these follow-up RPCs.
- Applied the one existing Postgres database migration manually before
  merge; no old-session migration is wired into daemon startup.
- Captured the implementation plans in
  `rust/docs/session-sync-redesign-plan.md` and
  `rust/docs/queued-message-mutations-plan.md`.

Verification:

- `cargo check --manifest-path rust/Cargo.toml`
- `cargo test --manifest-path rust/Cargo.toml`

## 2026-05-15

### Project-Scoped Sessions

- Added a `projects` Postgres table with UUID primary keys, display names, and
  per-project `starting_cwd`; every session now belongs to a project.
- Added project CRUD/list RPCs and threaded `project_id` through
  `session.start`, `session.list`, `session.get`, and session/fork summaries.
- Session config snapshots the project's `starting_cwd` when a session is
  created; model dynamic prompt context and local tool execution use that
  session cwd instead of one daemon-wide workspace. Updating a project cwd only
  changes the default for future sessions.
- Updated the web UI sidebar to select a project first, then list sessions
  nested under that project. Added project create/edit dialog with rename and
  starting-cwd fields.

Verification:

- `cargo check --workspace`
- `npm run build:web --silent`

## 2026-05-14

### Unify Shell Tool As `bash` For Both Providers

Replaced the diverging shell surfaces (OpenAI custom `shell` function tool and
Anthropic native `bash_20250124`) with a single custom function tool named
`bash`, registered identically for both providers via the existing builtin
tool registry.

Motivation: the daemon spawns a fresh `sh -lc` per call, so the persistent-
session contract Anthropic advertises for `bash_20250124` (and the `restart`
op the model is trained to use) was a misrepresentation of what the runtime
actually does. Going custom on both sides keeps the model's expectations
aligned with the runtime, removes the per-provider tool divergence for
shell, and lets the registry/display layer treat bash uniformly. The edit
tools (`apply_patch` for OpenAI, `text_editor_20250728` for Anthropic) stay
provider-native because their schemas are semantically rich enough to be
worth the provider training prior.

Changes:

- `agent-tools`: collapsed `BashTool`/`ShellTool` into one `BashTool` whose
  definition is registered for both providers. Schema is
  `{ command: string | argv, timeout_ms?: integer }` — the `workdir`
  override was removed since the daemon workspace is fixed at launch and
  the model can chain with `&&` or call `cd` inside the command instead.
- `agent-provider/openai.rs`: removed `openai_shell_tool()`. The OpenAI
  coding profile now renders `bash` and `grep` through one shared
  `openai_function_from_builtin` helper.
- `agent-provider/anthropic.rs`: replaced the `{type: bash_20250124, name: bash}`
  literal with the same custom-function rendering used for `grep`.
- `agent-daemon`: added a one-line nudge to the dynamic prompt context so the
  model is told explicitly that each bash call runs in a fresh shell. This
  hedges against Claude's prior toward `bash_20250124`'s persistence
  semantics, costs ~30 tokens, and sits in dynamic context so it does not
  perturb the cached stable prefix.
- Updated provider tests for the new tool ordering and SSE fixtures, plus
  the registry/display tests that asserted the old `shell`/`bash` split.

Rollout requires a DB migration. An untracked
`scripts/migrate_shell_to_bash.py` walks JSONB recursively in
`transcript_entries.item`, `transcript_entries.provider_replay` (including
nested `raw_json` strings from OpenAI function_call replay items),
`actions.payload`, and `events.payload`, rewriting `tool_name: "shell"`
and `name: "shell"` to `"bash"`. The procedure is: stop the daemon, drain
in-flight actions (or accept they will be marked stale), `pg_dump`,
`--dry-run`, then run for real. Anthropic-side sessions need no migration
because the wire-level tool name has always been `"bash"` — only the
wrapper `type` changes, and that lives in the regenerated request body.

Verification:

- `cargo check --workspace` passes.
- `agent-tools` (8 tests), `agent-provider` (28 tests), `agent-core`
  (22 tests), `agent-session` (69 tests), `agent-store` (2 tests),
  `agent-daemon` (3 tests), and `agent-vocab` (4 tests) all pass under the
  Apple SDK/linker environment.

## 2026-05-13

### Provider Replay Sidecar Cleanup

Moved provider replay out of semantic assistant messages. The old
`ProviderReplayRecord` shape made `AssistantMessage.items` carry both visible
assistant output and opaque provider-continuation payloads, which muddied the
core/session boundary and leaked provider concerns into UI-facing transcript
types.

Implemented the cleaner split:

- `AssistantItem` is now only visible `Text` or semantic `ToolCall`.
- `ProviderReplayItem` lives with provider vocabulary and stores
  `{ provider, raw_json }` for exact OpenAI Responses and Anthropic Messages
  replay.
- `StoredTranscriptEntry` and `TranscriptStorageNode` carry a
  `provider_replay` sidecar, so replay remains aligned with the append-only
  transcript tree without becoming a visible transcript item.
- `ModelContext` materializes `ModelContextEntry` values and preserves replay
  sidecars along fork/switch/compact branches.
- `ModelRequest` now passes `Vec<ModelTranscriptEntry>` to providers.
  Providers serialize replay sidecars when present and fall back to semantic
  transcript items for older or replay-free history.
- Postgres has a sibling `transcript_entries.provider_replay` JSONB column.
  The schema migration lifts legacy `provider_replay_record` assistant items
  into that column and removes them from assistant message JSON.
- The daemon attaches provider replay returned by model calls to the persisted
  assistant transcript entry in the same output batch.
- Web types no longer include a `provider_replay_record` assistant item; replay
  is available only as optional entry sidecar/debug data.

Verification:

- `cargo test --manifest-path rust/Cargo.toml --workspace --quiet` passes with
  the macOS SDK/linker environment.
- `npm run build:web --silent` passes.

## 2026-05-12

### Compaction And History Forest Proposal

Added `rust/docs/compaction-and-history-forest.md` to capture the proposed
model where rewind, fork, and compaction share the same transcript-forest
primitive. The key design choice is to make compaction append a new
model-visible root with a lineage pointer to the summarized source leaf, rather
than replacing the active path returned from the harness.

Implemented the first pass of that proposal:

- Added `TranscriptItem::CompactionSummary` with `source_session_id`,
  `source_leaf_id`, `summary`, `tokens_before`, and `last_turn_id`.
- Removed the generic `InjectedMessage` transcript path from vocabulary, core,
  session, providers, and web rendering. The only non-turn transcript context
  we currently need is typed compaction summary context.
- Removed session-owned replacement-context compaction:
  `CompactionState`, `SessionAction::RequestCompaction`,
  `SessionInput::CompactionCompleted`, and harness compaction RPC methods.
- Moved compaction completion to `agent-store` as a Postgres compare-and-set
  transaction. It inserts a compacted root with `parent_id = null`, updates
  `sessions.active_leaf_id`, marks the compaction action complete/stale/error,
  and emits transcript/history/compaction events atomically.
- Added a daemon-owned provider compaction path with no tools. It summarizes
  only the dynamic transcript/model context; the global stable system prompt
  remains provider configuration and continues to be rendered before transcript
  context on ordinary model calls.
- Updated the web transcript and picker types for `compaction_summary`. The
  main conversation renders the active branch only, while rewind/fork pickers
  can target entries across the full transcript tree.

Verification:

- `cargo check --workspace` passes.
- `CARGO_TARGET_AARCH64_APPLE_DARWIN_LINKER=/usr/bin/cc cargo test --workspace`
  passes. The explicit linker is required on this machine because the default
  profile `cc` is GCC and cannot find macOS `libiconv`.
- `npm run build` in `packages/web` passes.

## 2026-05-10

### Scope

The Rust rewrite is now aimed at a small personal-use pi implementation rather
than a Rust clone of the local hierarchical subagent experiment. The design
keeps modular boundaries because customizability is the main requirement, but
avoids broad plugin/general-user machinery until there is a concrete consumer.

### Design Decisions

1. Vocabulary moves into `agent-vocab`.
   Providers, tools, storage, core, sessions, and CLI all need the same message
   and transcript shapes. Keeping those types inside `agent-core` would make
   unrelated crates depend on the FSM just to talk about messages.

2. Thinking blocks are represented only as redacted markers.
   We do not need the content of model thinking blocks for this project. The
   vocabulary can record that hidden thinking occurred without storing the text
   or making it visible to later provider requests.

3. User content supports text and images; assistant output stays text/tool-call
   focused for now.
   The known hard requirement is image input. Assistant image generation and
   binary tool results are not part of the initial personal coding-agent loop.

4. Tool call ids are strings.
   Providers emit opaque ids. Numeric ids were convenient for early FSM tests,
   but string ids avoid lossy adapters and make storage/provider interop clean.

5. Storage started as a trait, but the Rust websocket path is now Postgres-only.
   The initial JSONL/in-memory experiment helped prove the session snapshot
   shape; once Postgres became the durable control-plane source of truth, the
   separate `SessionStore` abstraction was removed.

6. `agent-orchestrator` was removed.
   After demotion it only contained a live-session registry, so the separate
   crate name was misleading. The follow-on `SessionRegistry` and async channel
   runner were removed too; durable storage should be the session lookup layer.

7. Existing session semantics are preserved as the runtime foundation.
   `AgentSession` still owns resume, rewind, fork, compaction, open-tail crash
   recovery, queued-input preservation during history edits, and stale-work
   invalidation. The Rust rewrite can deviate from pi-mono where those semantics
   are better for this codebase.

8. Storage snapshots live at the session boundary.
   `agent-session` owns `StoredSession` and `StoredTranscriptEntry` plus
   conversion to and from live sessions. `agent-store` owns the concrete
   Postgres model that persists those semantics for the websocket daemon.

   Transcript timestamps are `u64` milliseconds. `SystemTime::as_millis()`
   yields `u128`, but JSON does not need 128-bit millisecond values and
   `serde_json` rejects `u128`; `u64` keeps the serialized snapshot shape
   portable.

9. Provider adapters start as complete-request adapters.
   Streaming can be normalized later inside `agent-provider`; the first Rust
   pass favors a small `ModelProvider::complete` surface that is enough for a
   local session loop.

10. Builtin tools are intentionally unsandboxed primitives.
    `read`, `write`, `edit`, and `bash` are enough for a personal coding loop.
    Tool calls are always allowed; there is no approval interface or tool policy
    in the Rust plan.

11. The CLI is a composition harness, not the product shell.
    `pi-rs` proves the crates compose: session, provider, and tools can drive a
    simple prompt. Durable named sessions and richer UX can be layered on later.

### Implementation Notes

- Added `agent-vocab`, `agent-store`, `agent-provider`, `agent-tools`, and
  `pi-cli` to the Rust workspace.
- Updated `agent-core` to consume structured `UserMessage`s and string tool
  call ids through `agent-vocab`.
- Added session-to-storage conversion:
  `AgentSession::to_stored_session` and `AgentSession::from_stored_session`.
- Added initial in-memory and JSONL `SessionStore` implementations, then later
  removed them when Postgres became the only supported durable backend.
- Added OpenAI and Anthropic provider adapters.
- Added a separate async tool registry with builtin local tools.
- Removed the `agent-orchestrator` crate.
- Removed the in-memory `SessionRegistry` and async channel `AgentRunner` from
  `agent-session`.
- Updated the Rust architecture docs and crate READMEs to reflect the new
  target.

### Verification

- `cargo fmt --manifest-path rust/Cargo.toml --all --check` passes.
- `cargo check --manifest-path rust/Cargo.toml --all` passes.
- `cargo test --manifest-path rust/Cargo.toml -p agent-core` passes: 22 tests.
- Full workspace tests pass with the Apple SDK/linker made explicit:
  `SDKROOT=/Library/Developer/CommandLineTools/SDKs/MacOSX.sdk RUSTFLAGS='-C linker=/Library/Developer/CommandLineTools/usr/bin/clang' cargo test --manifest-path rust/Cargo.toml --all`.
  The current suite runs 90 unit tests across the workspace plus doc-test
  harnesses. The default `cc` on this machine is a profile GCC that cannot find
  `libiconv`, so the explicit linker environment is required here.

### Websocket/Postgres Plan

- Added `rust/docs/websocket-rpc.md` as the source of truth for the
  frontend-facing control plane.
- Decision: no user-facing `open`, `close`, `resume`, or `delete` session RPC.
  Sessions are durable Postgres records; websocket connections subscribe to
  them but do not define their lifecycle.
- Decision: no approval interface. Tool requests always run, and interruption
  is turn-level through `input.interrupt`.
- Decision: do not add a rich persisted session status enum. The frontend gets
  a derived `activity` of `idle`, `queued`, or `running`; interrupted/crashed
  are turn outcomes in transcript history.
- Decision: an open transcript tail is valid while external work is running
  when pending action rows explain it. Interrupt should be atomic at the
  repository level: either the pre-interrupt open tail remains recoverable, or
  the post-interrupt tail is closed with `TurnFinished(Interrupted)`.
- Decision: daemon death is recovered at Postgres transaction boundaries. A
  restarted daemon repairs open tails and stale action attempts; external tool
  side effects are not transactional and may survive even when their action is
  later marked stale/error.
- Decision: move to Postgres before implementing websocket RPC. Postgres should
  hold sessions, transcript entries, queued inputs, action state, and event
  logs. A transient in-memory `AgentSession` may exist while actively driving a
  turn, but it is not source-of-truth staging.
- Decision: source-mutating history writes are idle-only in the websocket
  contract. Rewind, active-leaf mutation, and compaction should fail with
  `session_busy` while a turn is running; users must interrupt first and retry
  after idle. Fork is allowed while the source is running when it targets an
  explicit committed turn boundary, because it does not mutate the source.
- Decision: websocket validation should emulate real frontend/user behavior.
  Scenario scripts should send websocket RPC, observe events, and verify
  persisted Postgres consequences. Dev harness controls are only for forcing
  substantial lifecycle edges that would otherwise be timing-sensitive; provider
  runs through OpenAI/Anthropic should assert real protocol consequences, not
  merely connection success.
- Added a manual websocket exercise runbook with concrete JSON frames for
  basic turns, image inputs, steer/follow-up ordering, interruption, real tool
  success/error behavior, parallel tool ordering, rewind, running-safe fork, invalid
  fork targets, compaction validity, event replay, crash recovery, and real
  provider behavior.
- Added the documentation-sync rule to the implementation sequence so crate
  boundaries, RPC methods, lifecycle rules, and storage invariants are reflected
  across the READMEs, architecture docs, websocket RPC docs, and worklog.
### Websocket/Postgres Implementation

- Added `agent-daemon` with the `pi-agentd` binary.
- Added a concrete Postgres repository for the websocket path. It migrates and
  writes normalized `sessions`, `transcript_entries`, `queued_inputs`,
  `actions`, and `events` tables.
- Kept Postgres as the only durable websocket backend. The original
  backend-neutral `agent-store` layer was later collapsed into a concrete
  Postgres crate, while snapshot shapes moved to `agent-session`.
- Implemented websocket RPC for session creation/list/get/configuration,
  event subscription, follow-up/steer/interrupt input, history tree/context/
  rewind/fork, tool listing, compaction request, and model/compaction harness
  controls.
- Implemented the simplified lifecycle model: no explicit open/close/resume/
  delete, no approval RPC, no persisted session status enum beyond derived
  `idle`/`queued`/`running`.
- Implemented Postgres as the consistency boundary. Accepted transitions commit
  transcript entries, action rows, queued-input updates, active-leaf changes,
  and events together.
- Added per-action `attempt_id` and guarded completions so late model/tool/
  compaction results from stale attempts cannot mutate transcript history.
- Recovery now runs before first touch of an idle-looking session. If the
  daemon died with pending work, recovery marks unfinished actions stale,
  appends recovered transcript entries, emits transcript/turn events for those
  entries, and emits `session.recovered`.
- Fork is allowed while the source is running if the target is an explicit
  committed turn boundary. Fork from `null` is rejected.
- Rewind, session configuration, and websocket compaction request are idle-only.
- Tools run automatically through the builtin registry. Tool-returned failures
  become error `ToolResult`s and `error` action rows; there is still no approval
  or denial path.
- `AssistantItem` now serializes with explicit object tags so websocket/harness
  assistant messages and stored transcript JSON are stable:
  `text`, `thinking_redacted`, and `tool_call`.
- `OpenAiProvider` now supports two wire APIs:
  OpenAI Chat Completions for API-key use, and streamed Responses API for the
  ChatGPT/Codex backend.
- Added `ModelRequest::prompt_cache_key`; `agent-daemon` maps
  `provider.prompt_cache.key` to that field.
- `agent-daemon` reads Codex ChatGPT credentials from `CODEX_ACCESS_TOKEN` or
  `~/.codex/auth.json`, including `tokens.account_id`.
- Moved system prompt configuration out of sessions. It now lives in global
  daemon config exposed by `config.get` / `config.set`; provider dispatch reads
  that global prompt for model requests.
- Added a TypeScript websocket UI in `packages/web`, borrowing the dense
  three-pane session/log/inspector feel from `~/bigband/web` while keeping this
  project session-only. There is no task abstraction in the frontend.
- Frontend slash commands are thin websocket calls over the real RPC contract:
  `/new`, `/refresh`, `/status`, `/steer`, `/interrupt`, `/rewind`, `/fork`,
  `/compact`, `/context`, `/tree`, `/system`, `/provider`, and `/tools`.
  `/system` reads and writes global daemon config, not session config.
- `/rewind` and `/fork` now open history pickers instead of accepting raw
  transcript ids. The picker is the only web UI path for those history
  mutations; `/fork [title]` may prefill a title, but the branch point is still
  selected from visible turn context.
- Fork semantics were loosened from "committed boundary" to "any existing
  transcript entry." Rewind and compaction remain source-mutating operations
  that are constrained to idle/boundary-safe states; fork is source-non-mutating
  and can safely copy a partial path. Child sessions created from a non-boundary
  point close that copied tail as `Interrupted`, keeping the child runnable
  without treating the deliberate fork as daemon crash recovery.
- Tightened the websocket idle gate so source-mutating operations also reject
  sessions with queued-but-not-yet-consumed input. In user terms, idle means
  between turns after active work and queued input have drained.
- Removed the frontend's generic "message queued" and "steer queued" transcript
  notices. They were only RPC acceptance acknowledgements for durable queued
  inputs, not conversation events, and they surfaced confusingly while the agent
  was still running.
- Current-session picker testing caught a hydration race: invoking `/fork` or
  `/rewind` immediately after selecting a session could open against an empty
  local `entries` array. The slash handlers now refresh the expanded
  `session.get` snapshot before displaying either picker and pass the freshly
  loaded entries directly into the dialog instead of relying on React state
  flush timing.
- Restored slash autocomplete as a shallow command-name helper. While typing a
  partial command, Enter accepts the highlighted completion and inserts a
  trailing space; because the menu only appears for a bare command token, the
  next Enter submits and opens the picker/action. Exact commands such as
  `/fork` still submit immediately.
- Added command metadata for required arguments. Exact `/steer` now completes
  to `/steer ` instead of executing with an empty message, and submitting a
  required-argument command without arguments leaves the composer intact with a
  usage notice.
- Browser testing found that notices were hidden when no session was selected;
  the empty transcript view now still renders local notices such as `/help`
  output and connection errors.
- Browser testing also surfaced dev-only noise from the Vite page: a missing
  favicon request and React StrictMode's intentional websocket mount/unmount
  cycle. The web app now declares an empty favicon and renders once in dev so
  the websocket connection is not opened and immediately closed by StrictMode.
- User screenshot review caught assistant text overlapping the copyable entry id
  gutter. Assistant/tool transcript rows now use a grid with a reserved id
  column instead of placing the id absolutely over content.
- Transcript rendering now hides turn-start, graceful turn-finish, and
  tool-call-start bookkeeping entries. Assistant tool calls render as compact
  collapsible rows with the matching tool result folded into the same row,
  following the Bigband/Claude Relay pattern instead of exposing raw event
  records.
- The web client intentionally keeps no durable RAM staging layer. It subscribes
  to transient session events, refreshes expanded `session.get`, and lets
  Postgres-backed daemon state remain the source of truth.
- Duplicate websocket input retries with the same `client_input_id` now return
  the original queued input without emitting a second `input.queued` event.
- Queued inputs now remain durable `queued` rows while external model/tool/
  compaction work is unfinished. They are only marked `consumed` in the same
  transaction that materializes the next transcript turn, so daemon death cannot
  lose accepted input sitting only in the live session projection.
- If Postgres persistence fails after a live session advances, the daemon evicts
  that live session so the next interaction reloads from durable storage instead
  of treating RAM as a recovery source.
- Added `docs/design-decisions.md` to document both visible choices (session-only
  UI, slash commands, global system prompt, no approvals) and invisible choices
  (Postgres authority, queue durability, idempotency, recovery, provider/tool
  boundaries).

### Manual Websocket Verification

The daemon was exercised against a real Postgres 16 container through OrbStack:

```sh
DOCKER_HOST=unix:///Users/schwinns/.orbstack/run/docker.sock docker run -d \
  --name pi-relay-pg \
  -e POSTGRES_PASSWORD=postgres \
  -e POSTGRES_USER=postgres \
  -e POSTGRES_DB=pi_relay \
  -p 55432:5432 postgres:16-alpine
```

Manual websocket scenarios completed:

- Basic harness turn, event subscription, replay, `session.get`, and
  `history.context`.
- Global system prompt persistence, update, and clearing with `null`.
- Text and image content persistence through Postgres and context
  materialization.
- Follow-up/steer queue ordering and duplicate `client_input_id` idempotency.
- Interrupt of pending model work, stale harness completion rejection, and
  interrupted turn closure.
- Running rewind rejection, post-interrupt rewind success, and non-boundary
  rewind rejection.
- Running-safe fork from a previous boundary, fork-from-null rejection, and
  open-turn fork rejection.
- Real `read`, `write`, `edit`, and `bash` tool execution, including tool
  errors and parallel tool requests.
- Valid compaction completion and invalid compaction error persistence.
- Daemon death/restart recovery with stale actions, crashed turn tail,
  `turn.finished`, and `session.recovered` replay.
- Event replay high-water behavior: `events.subscribe(after_event_id)` returned
  buffered replay while a session was active without duplicating those events
  as live frames.
- Real Codex text turn through websocket using `~/.codex/auth.json`.
- Real Codex image-URL turn through websocket using the streamed Responses API.

Anthropic websocket smoke was not run because no raw `ANTHROPIC_API_KEY` was
available in the environment; the local Claude Code credentials were present
but not exposed as an Anthropic API key.

### Codex Auth Recovery

New session creation was verified in the browser and did not crash the daemon.
The failing "new session" case was a first-turn provider failure: the Codex
Responses request returned HTTP 401 from `chatgpt.com/backend-api/codex`, and
the session FSM correctly closed that turn as `Crashed`.

Implementation decision: provider credentials are reloaded for every model
request instead of being captured once at daemon startup. This keeps Postgres
session state independent from process-local auth state and lets a refreshed
`~/.codex/auth.json` take effect without recreating a session.

Implementation decision: when the Codex provider returns 401, the daemon uses
the same ChatGPT OAuth refresh endpoint and client id as upstream Codex, writes
the refreshed tokens back to `~/.codex/auth.json`, and retries the model request
once. There is no broad retry loop or fallback provider; a second failure is
persisted as the real model error.

UI decision: live `model.error` events are surfaced as visible notices. The
transcript may still show a crashed turn when the model fails, but old provider
error notices are not durable session history.

Follow-up browser finding: after `/new <title>` the React selected-session state
changed, but the mutable ref used by immediate sends could still contain the
previous session id until the next render effect. The UI now updates both at the
same time for slash-created sessions, forks, initial selection, and manual row
selection. This prevents a fast follow-up after session creation from landing in
the previously selected session.

Verification:

- `npm run build:web` passes.
- `cargo test --manifest-path rust/Cargo.toml -p agent-daemon -p agent-provider`
  passes with the explicit Apple SDK/linker environment.
- Full workspace tests pass with the same explicit linker environment: 91 unit
  tests plus doc-test harnesses.
- A real websocket smoke created a Codex `gpt-5.5` session, sent
  `input.follow_up`, observed `turn.finished` with `Graceful`, and got
  `auth smoke ok` from the model. The smoke session was deleted afterward.
- A real headed Chrome smoke created a session through `/new <title>`, sent the
  next message from the composer, and Postgres showed the message, assistant
  reply, and graceful turn in the newly created session. That smoke session was
  deleted afterward.

### Markdown And HTML Rendering

Assistant text is rendered through `react-markdown` with GitHub-flavored
Markdown enabled. The web app styles headings, lists, links, inline code,
fenced code blocks, blockquotes, tables, and horizontal rules inside the
existing compact session-log layout.

Raw HTML embedded in assistant Markdown is parsed with `rehype-raw`, so passive
HTML elements such as `<details>`, custom table markup, and inline semantic
tags render as DOM. Script execution is not enabled in the main app origin; if
active HTML/JS artifacts become useful, they should run in a sandboxed preview
surface rather than inside the transcript log.

Verification: a disposable harness session rendered Markdown headings, bold
text, list items, fenced code, a raw-HTML `<details>` block, and a raw-HTML
table cell in headed Chrome without console errors. The smoke session was
deleted afterward.

### Real Browser Verification

The Vite UI was driven in headed Google Chrome through Playwright Core against
the live websocket daemon and the same Postgres database. Screenshots from the
run were written under `/tmp/pi-relay-browser.4B6Thz/screens`.

Browser-driven scenarios completed:

- Initial load, websocket connection, and `/help` before selecting a session.
- `/new` session creation and automatic selection.
- Global `/system` update and clear, including inspector visibility.
- `/provider codex gpt-5.5` with metadata preservation verified through RPC.
- Plain composer follow-up, `/steer`, harness model completions, and visible
  transcript order: first follow-up, steer, then queued normal follow-up.
- `/status`, `/context`, `/tree`, `/tools`, and `/refresh` visible notices.
- Running-safe `/fork <boundary>` while the source session had a running model
  action.
- `/fork root` rejection.
- Busy `/rewind root` rejection while a model action was running.
- `/interrupt` followed by successful `/rewind root`.
- Idle `/fork <boundary>` followed by `/rewind root` on the child.
- Copyable transcript entry ids rendered in the browser.
- Updated web history behavior to test `/rewind` and `/fork` as dialog-driven
  picker flows rather than raw boundary-id slash commands.
- Updated transcript rendering checks to expect hidden turn bookkeeping and
  compact tool rows with folded results.
- Added fork-any-entry coverage to keep rewind boundary rules separate from
  fork branch-copy rules.
- Added queued-state lifecycle coverage: a queued-but-not-consumed input makes
  `history.rewind` return `session_busy`, so idle really means no active or
  queued user work.
- Browser console was treated as a failure signal after filtering nothing; the
  final run passed without console errors or warnings.
- Follow-up cleanup removed the browser/manual verification sessions from the
  shared local Postgres database. Future browser verification should either use
  a disposable database or clean up tagged test sessions immediately so the real
  UI does not show verification debris.

### Review Notes

Subagent review focused on simplicity, modularity, and avoiding brittle fallback
paths. Changes made from that review:

- Follow-on dispatches after model/tool completions are no longer dropped.
- Interrupt no longer inserts a durable cancel action row that leaves a session
  busy; it bulk-updates unfinished work and emits `session.work_cancelled`.
- Queue consumption is committed atomically with the transcript/action/event
  transition that consumed it.
- Recovery runs before idle-gated mutations.
- Stale completion checks use persisted attempt identity.
- Event replay now tracks a per-socket high-water mark so replayed buffered
  events are not duplicated by live broadcasts.
- Documentation now describes the actual harness surface and the real provider
  credential paths.

### Draft Sessions, Queue Claims, And Picker Targets

- Added `session.start` for the web draft path. It creates a durable session,
  records `session.created` plus `input.accepted`, materializes the first user
  input into transcript/action/event state, and only then dispatches follow-on
  work. Retrying with the same stable draft-owned `session_id` returns the
  existing session instead of creating another row.
- Changed ordinary idle `input.follow_up` handling so messages are fed directly
  into the session when no work or queued backlog exists. Busy-session inputs
  still use `queued_inputs`; visible steering is queued-row promotion.
- Hardened queued-input consumption: the session driver claims a row as
  `consuming` with a claim id, and the final `consuming -> consumed` update is
  validated inside the transcript/action/event transaction.
- Added abandoned-claim recovery by resetting `consuming` rows to `queued` when
  a session is first touched after daemon restart.
- Extended `history.fork` with `placement: "before"` for user-message fork
  targets while keeping fork-from-null invalid.
- Changed the web UI so New session and `/new` create local `localStorage`
  drafts. Drafts survive refresh, appear in the sidebar, and disappear only
  after `session.start` returns a durable session.
- Added web-owned composer draft storage for existing sessions. Rewind/fork
  user-message targets restore the historical message into the composer without
  writing that draft into core session tables.
- Updated the rewind/fork picker to operate on visible targets. Rewind maps
  user messages to the previous safe boundary/root; fork maps user messages to
  `placement: "before"` and maps completed assistant responses to the enclosing
  completed turn boundary.
- Added a `metadata.hidden = true` list filter so local verification sessions
  can be removed from the sidebar without inventing a durable delete/open/close
  lifecycle.
- Kept documentation in sync across `websocket-rpc.md`,
  `design-decisions.md`, `architecture.md`, and
  `draft-sessions-and-history-plan.md`.

### Atomic Input Ledger And Branch-Aware Pickers

- Finished the `client_input_id` idempotency path for idle inputs. Immediate
  `input.follow_up` and `session.start` now record a consumed input ledger row
  in the same Postgres transaction that appends transcript, action, active-leaf,
  and event state. Retrying a lost websocket response returns the durable record
  instead of appending a duplicate user message.
- Added optional `expected_active_leaf_id` validation for user input and
  `history.rewind`. The web UI records the base active leaf for composer drafts
  and sends it back so restored historical edits cannot silently land on a
  newer branch.
- Serialized source-mutating session operations through the per-session
  `SessionDriver`. Rewind, configure, and compaction now share the same
  source-mutation critical section as input driving instead of relying on a
  separate RAM staging model.
- Tightened queued-input failure handling: if a claimed queued input cannot be
  fed into the session, the daemon moves that exact claim back to `queued`
  before returning the input error.
- Updated the web history picker to compute targets from transcript parent
  chains rather than the raw append order. Rewind options are active-branch
  boundary/user-message choices; fork options can target any explicit entry.
  Forking from a user message still uses `placement: "before"`, while assistant
  and tool targets fork from the exact selected entry.
- Removed the bare root rewind option from the UI picker. The first user
  message target still rewinds the backend to root, but restores that message
  into the composer so the user never lands on an empty pre-message state by
  accident.
- Kept slash command text out of draft persistence. Slash autocomplete still
  helps discovery: Enter completes a partial command, and the next Enter
  executes commands such as `/rewind` or `/fork`.
- Browser verification passed against the live daemon: Markdown and raw HTML
  assistant rendering, slash autocomplete, picker-only rewind/fork, composer
  restoration, forked child composer restoration, and unsent local draft
  survival across reload.
- Manual websocket verification passed against the live Postgres daemon:
  `session.start` replay, immediate input replay, queued replace/cancel,
  queued consumption replay, stale active-leaf rejection, busy rewind rejection,
  fork-from-running-source, fork-from-null rejection, interrupt then rewind,
  and daemon restart recovery to a crashed idle turn.

### Active-Branch Transcript Rendering

- Fixed the main web transcript to render the active root-to-leaf branch rather
  than every row in the append-only transcript forest. Rewind still preserves
  abandoned rows durably for history/fork operations, but off-branch UI elements
  no longer remain in the visible conversation.
- Browser verification created a two-turn harness session, rewound to before
  the second user message through the picker, and confirmed the second turn
  disappeared from `.message-scroll` while the historical user message was
  restored into the composer.

### Queue Pane And Stop Control

- Removed `/steer` and `/interrupt` from the web slash command surface. Normal
  composer submits now always send `input.follow_up`; active interruption is a
  stop button next to the composer.
- Added `input.promote_queued` to promote a still-queued follow-up to steer
  priority. Promotion records `origin.promoted_at`, and the queue consumes
  steers by promotion order before unpromoted follow-ups by creation order.
- Added `queued_inputs` to `session.get` snapshots so the web UI can render a
  small composer-adjacent queue pane. Follow-up rows show a steer button;
  promoted rows show as steering and cannot be promoted again.
- Verified by websocket that three queued follow-ups promoted in the order
  "two" then "one" were consumed as `two`, `one`, then the unpromoted third
  follow-up.
- Verified in a real browser that pressing Enter during a running turn creates
  visible queued follow-up rows, the row-level steer button promotes one row,
  and the stop button interrupts a running turn to an idle interrupted tail.

### Agent Daemon Decomposition

- Split the 3.6k-line `agent-daemon/src/main.rs` into focused modules:
  `config.rs`, `types.rs`, `auth.rs`, `codec.rs`, `provider_runtime.rs`,
  `runtime.rs`, and `state.rs`, with persistence now owned by `agent-store`.
- Kept websocket routing and RPC handlers in `main.rs`; moved live session
  driving, dispatch, and recovery into `runtime.rs` so handler code no longer
  owns the turn lifecycle.
- Moved Postgres SQL and transaction helpers out of daemon control flow, first
  into a daemon repository module and then into `agent-store::PostgresAgentStore`.
- Moved Codex credential refresh into `auth.rs` and provider selection into
  `provider_runtime.rs` so provider execution is decoupled from websocket RPC.
- Documented the daemon module boundaries in `architecture.md` and
  `design-decisions.md`.

### OpenAI Chat Completions Request Policy

- Updated the OpenAI Chat Completions renderer to set the request policy fields
  we always want for this personal agent path: `parallel_tool_calls = true`,
  `service_tier = "priority"`, `store = false`, and
  `prompt_cache_retention = "24h"`.
- Forwarded `ModelRequest::prompt_cache_key` on Chat Completions and added a
  stable default cache key when session/provider config does not specify one.
- Switched the Chat Completions token cap field from `max_tokens` to
  `max_completion_tokens` to match the current request surface.
- Added provider tests for the Chat Completions body so these defaults remain
  visible and intentional.
- Documented the prompt-caching layout implication: keep the cacheable global
  system/tool/project prefix before dynamic session/user context.
- Added `PromptSections` to provider requests. The global system prompt is now
  the stable prefix, daemon runtime context is rendered after it, and transcript
  history follows both sections.
- Kept the stable/dynamic split internal. Provider rendering appends runtime
  context plainly after the stable prefix rather than adding a model-facing
  "Dynamic Context" heading.
- Removed the daemon and CLI default output-token caps for OpenAI/Codex
  requests. `provider.max_tokens` is now only an explicit opt-in cap; Anthropic
  keeps a provider-local fallback because its Messages API requires the field.

### Postgres Store Consolidation

- Removed the old `agent-store` in-memory/JSONL `SessionStore` layer. Postgres
  is now the only supported durable backend.
- Moved the concrete Postgres repository from `agent-daemon` into
  `agent-store::PostgresAgentStore`, including schema migration, session
  configuration, transcript persistence, queued input ledger, actions, events,
  recovery helpers, and global daemon config.
- Moved `StoredSession` and `StoredTranscriptEntry` into `agent-session`, where
  they describe live-session snapshots rather than a pluggable backend API.
- Slimmed `agent-daemon` further: it now owns websocket routing, auth,
  provider/tool dispatch, recovery orchestration, and live session driving, but
  not the SQL implementation.
- Split store imports by ownership: `agent-store` depends directly on
  `agent-vocab` for message/config vocabulary and on `agent-session` for
  session snapshots and actions.
- Cleaned the local Postgres database of 37 disposable verification sessions.
  The remaining visible sessions are the two real-looking web sessions.
- Moved the Postgres store from `tokio-postgres` to SQLx. `PostgresAgentStore`
  now uses `PgPool`, SQLx transactions, SQLx bind parameters, and typed row
  decoding while keeping SQL visible for the transaction-heavy recovery and
  ledger logic. Diesel/SeaQuery are credible packages, but they add more
  abstraction than this JSONB-heavy store currently needs.

### Typed Wire Vocabulary

- Replaced ad hoc Rust `String`/`&str` control-flow checks for the small closed
  vocabularies with enums: `ProviderKind`, `InputPriority`,
  `QueuedInputStatus`, `ActionKind`, `ActionStatus`, `SessionActivity`, and
  `EventType`.
- Kept the Postgres and websocket representation unchanged by serializing those
  enums to the existing wire strings. Existing `anthropic` provider configs are
  accepted as a legacy alias and normalized to `claude`.
- Typed the daemon RPC boundary for websocket method dispatch and fork
  placement, while preserving the existing `unknown_method` and
  `invalid_placement` error behavior.
- Simplified the daemon codec by deserializing user content blocks, image
  sources, and assistant items through the existing serde-tagged vocabulary
  types instead of manually matching `"type"` and `"kind"` strings.
- Added focused unit coverage for enum round-tripping, legacy provider alias
  parsing, invalid storage vocabulary rejection, RPC method parsing, and fork
  placement parsing.

### Empty Draft Cleanup

- Confirmed the "extra empty sessions" were browser-local draft rows, not
  durable Postgres sessions. The earlier Postgres cleanup removed disposable
  verification sessions while preserving real web sessions with transcript
  history.
- Added UI-owned cleanup for empty unsent local drafts. Creating a new draft
  now collapses abandoned empty drafts, and selecting another session deletes
  the current draft if its composer is still empty.
- Kept the refresh behavior we wanted: one empty unsent draft can still survive
  browser refresh while it is the active local draft. Drafts with typed composer
  content remain durable browser-local state until sent or cleared.

### Transcript Notice Placement

- Stopped rendering transient UI notices inside the transcript stream. The
  message list now renders only branch-filtered transcript entries plus the
  live activity pill.
- Moved success/error/info notices to a fixed toast stack outside the transcript
  pane, with inspector-aware positioning and automatic expiry.
- Removed the `draft created` notice entirely. Draft creation is represented by
  the active local draft/sidebar row, and surfacing it as a transcript-adjacent
  green line made it look like durable agent history.

### Codex Residency Header

- Diagnosed the web-visible Codex `401 Unauthorized` failure. The ChatGPT/Codex
  token refresh path was working, but the backend response body said
  `Workspace is not authorized in this region.`
- Compared the request with upstream Codex and confirmed the missing routing
  signal was the `x-openai-internal-codex-residency: us` header.
- Added that residency header to every Codex provider request alongside the
  bearer token and `ChatGPT-Account-ID`.
- Verified the same refreshed `~/.codex/auth.json` credentials return a live
  `200` stream from `https://chatgpt.com/backend-api/codex/responses` when the
  residency header is present.

### Rewind Picker Simplification

- Removed duplicate "after turn" choices from the web rewind picker. The only
  visible rewind path is now choosing a historical user message, rewinding to
  the boundary before that message, and restoring that message into the composer
  for editing.
- Left the Rust RPC and storage model unchanged. Rewind remains the low-level
  "set active leaf to a turn boundary/root" operation, while fork remains the
  source-non-mutating "copy from any transcript entry" operation. The frontend
  maps both operations from the same transcript-entry history data but applies a
  narrower rewind filter.

### Kernel Simplification Pass

- Made `agent-vocab` the canonical import path for IDs, messages, transcript
  items, provider config, and tool-result vocabulary. `agent-core` now exports
  only `AgentInput`, `TurnInput`, `AgentAction`, and `AgentCoreLoop`.
- Removed `TurnOrigin` and the tagged steer/follow-up constructors. The core
  now distinguishes only user input from generic injected context; it carries no
  subagent source/kind routing metadata.
- Kept `InjectedMessage.kind` as transcript vocabulary for caller-authored
  notes such as compaction summaries, not as a subagent protocol.
- Collapsed the session model-completion path so provider completions with
  token-count updates must use `SessionInput::ModelCompleted`; direct
  `AgentSession::enqueue_input(AgentInput::ModelCompleted { .. })` is rejected.
- Simplified live tool state in `agent-core` from parallel vectors into one
  per-tool slot containing the call, action id, and optional result.
- Trimmed `TranscriptStore` by removing unused public helpers such as
  `entry_count` and the boundary-copy helper. Active path enumeration is now an
  internal materialization detail.
- Removed the remaining duplicate completion-correlation logic. Model/tool
  completions now compare through one `CompletionTarget` shape used by session
  actions and outstanding action tracking.
- Removed `SessionInput::Agent`; plain agent inputs now enter only through
  `AgentSession::enqueue_input`, while session-owned inputs such as model
  completions with context tokens enter through `enqueue_session_input`.
- Folded daemon recovery/loading helpers into `SessionDriver` so there are no
  parallel free-function and driver-method paths.
- Collapsed Postgres event insertion to one SQLx helper shared by pool and
  transaction callers.
- Made compaction invalidation account for a second compaction request that
  arrived while a compaction was already running. Invalidation still cancels
  both the active request and that queued follow-up, but `AbandonedCompaction`
  now carries the discarded-request fact and the failed event says so.
- Removed the unused `ModelContext::from_transcript_items_closing_open_turn_as_crashed`
  convenience helper and the public `AgentSession::from_transcript_items`
  wrapper. Tests now compose through `ModelContext::from_transcript_items` and
  `AgentSession::from_model_context` directly.

### Review Phase 2-4 Follow-Up

- Made spawned model/tool completion failures observable. Provider/tool domain
  failures still flow through normal session inputs, but infrastructure
  failures from stale attempts, persistence, or post-completion driving now log,
  mark the action stale, and emit an error event instead of leaving a running
  action row behind.
- Hardened websocket handling so malformed JSON returns an RPC error frame
  without dropping the connection. Broadcast lag now replays missed events from
  Postgres using the per-subscription high-water cursor.
- Configured the Postgres pool explicitly and added a shutdown drain for
  in-flight dispatch tasks before closing the pool.
- Pruned the websocket RPC surface: empty durable session creation, direct
  `input.steer`, and queued replace/cancel RPCs are gone. User-visible steering
  is only queued follow-up promotion.
- Expanded `session.get` with `include_entries=true` so the web client can
  hydrate snapshot and transcript with one RPC. `history.tree` remains as a
  manual/debug endpoint.
- Wired real event replay in the web client by tracking `last_event_id` per
  session and passing it into `events.subscribe` after reconnect/session
  switches.
- Moved the visible composer into `composer.tsx`, added a `RpcClient`
  interface, removed the `composerHydrating` requestAnimationFrame gate,
  centralized slash-prefix parsing, generated fresh `client_input_id`s per send,
  validated stored composer drafts, and tightened the TypeScript wire unions.
- Pruned completed dispatch task handles as new dispatches are registered so
  the graceful-shutdown drain tracks live/recent work instead of growing for the
  entire daemon lifetime.
- Re-verified the pass with the Rust test suite, the web production build, the
  web unit tests, a manual websocket replay/bad-frame script, and a real browser
  smoke flow for markdown rendering plus rewind/fork dialogs.

### Append-Only Compaction Forest Pass

- Implemented the `rust/docs/compaction-and-history-forest.md` proposal as the
  active compaction model: compaction now appends a typed
  `TranscriptItem::CompactionSummary` root instead of replacing the active
  transcript path.
- Removed generic `InjectedMessage` vocabulary and the session-owned
  replacement-context compaction FSM. `agent-session` treats compaction
  summaries as valid boundary roots and otherwise stays focused on turn
  mechanics, rewind, and fork.
- Kept the configured global system prompt out of compaction input. Normal
  provider turns still render the stable prompt first, then dynamic daemon
  context, then transcript history; compaction summarizes only the dynamic
  transcript/model context materialized from the chosen active leaf.
- Made manual compaction a durable Postgres action barrier. Queued follow-ups
  can be accepted while compaction runs, but they are not consumed into the
  transcript until the compaction action completes or fails.
- Fixed daemon recovery around that barrier. A clean boundary with unfinished
  action rows is valid live work, not transcript corruption. On daemon startup,
  leftover unfinished actions are marked stale because provider/tool futures
  cannot survive the process boundary; open transcript tails are then repaired
  on first touch with a crashed turn boundary.
- Verified with `cargo check --workspace`, `CARGO_TARGET_AARCH64_APPLE_DARWIN_LINKER=/usr/bin/cc cargo test --workspace`,
  and `npm run build`.
- Verified against a real Postgres database and websocket client: two harness
  turns were compacted by the Codex provider into a `compaction_summary` root,
  the summary preserved transcript facts without leaking a stable global prompt
  marker, a follow-up queued during compaction was consumed only after
  `compaction.completed`, and a simulated daemon restart marked an unfinished
  model action stale then repaired the open turn as crashed.
- Dropped the temporary `pi_relay_forest_*` verification databases after the
  manual websocket checks.

### Web Draft Removal And Tree Picker

- Removed web-only draft sessions and durable composer drafts from the React
  state model. The sidebar now lists only durable Postgres sessions, and "New
  session" simply clears the selected session so the first real composer send
  creates the durable session through `session.start`.
- Added a startup cleanup for the old localStorage draft keys. The UI no longer
  reads those keys, so stale browser-local drafts cannot reappear as fake
  sessions.
- Kept rewind/fork editing as a transient composer convenience: selecting a
  historical user message restores its text into the visible composer, but the
  transcript forest remains the source of truth.
- Reworked the rewind/fork picker to render the transcript forest as a tree,
  including sibling branches and active-path highlighting. Rewind enables only
  editable user-message targets; fork can branch from selectable transcript
  points.
- Added a delayed refresh after terminal activity events so the UI has another
  chance to observe derived `idle` state if the immediate event-driven refresh
  lands while the daemon is still draining follow-on work.
- Verified with `npm run test` and `npm run build` in `packages/web`.

### Real Browser History Flow Verification

- Drove Chrome through the web app against a fresh Postgres database and the
  real Codex provider. The run used real composer input, send/stop button
  clicks, slash autocomplete, rewind/fork picker rows, provider-backed
  compaction, and real bash tool calls.
- Verified normal multi-turn output, slash autocomplete followed by execution,
  idle rewind-to-edit, fork-before-user with restored composer text, manual
  compaction into a `compaction_summary` root, post-compaction follow-up, fork
  from the compaction summary, and rewind from the compacted forest back into a
  pre-compaction user message.
- Verified fork while the source session was running. The source turn continued
  independently while the child session accepted its own real provider turn.
- Found and fixed the running rewind failure. `CancelSessionWork` is a
  session-wide invalidation event, not a persisted model/tool action row; the
  store now records `session.work_cancelled` directly instead of routing it
  through `action_payload`. That keeps interrupt persistence atomic and
  prevents the active runtime from being dropped before the interrupted tail is
  committed.
- Late model/tool completions now check whether their exact action attempt is
  still completable before requiring an active runtime. After interrupt or
  rewind, stale provider/tool completions are ignored without emitting a
  spurious `model.error` or `tool.error`.
- Verified running rewind after the fix: the UI interrupted the active turn,
  rewound to the selected user message, restored composer text, sent the
  replacement, and kept the abandoned running branch off the active path.
- Verified compact-while-running rejection and the stop button. Running compact
  did not create a new compaction action, and stop committed an interrupted
  turn plus `session.work_cancelled`.
- Verified queued follow-ups and per-row steer promotion in the queue pane with
  a long real bash tool call. The promoted row persisted as `steer`, and after
  stopping the active turn the steer message was consumed before the ordinary
  follow-up.
- Final verification passed with `npm run test`, `npm run build`, and
  `SDKROOT=$(xcrun --show-sdk-path) MACOSX_DEPLOYMENT_TARGET=15.0
  CC=/Library/Developer/CommandLineTools/usr/bin/clang
  CARGO_TARGET_AARCH64_APPLE_DARWIN_LINKER=/Library/Developer/CommandLineTools/usr/bin/clang
  cargo test`.

### Fork Preserves Session Forests

- Adjusted `history.fork` so a child session receives the source session's full
  transcript forest snapshot, not only the selected root-to-leaf path. The
  child active leaf still points at the requested fork target, and non-boundary
  targets still get an appended interrupted tail in the child.
- This keeps compaction roots and pre-compaction branches navigable inside a
  forked session. Compaction remains a node/root inside a session boundary; it
  is not treated as a session boundary or as a destructive replacement.
- Updated websocket and architecture docs to state the whole-forest fork
  behavior explicitly.

### Same-Session History Switching

- Added a `/switch` picker in the web UI. It uses the same transcript tree as
  rewind/fork. Selecting a completed turn or `compaction_summary` root moves
  the active leaf inside the same session through `history.rewind`; it does not
  create a new session.
- Verified in the live browser against the Postgres-backed UI database:
  `/switch` selected the compaction root inside the original session, then
  selected a pre-compaction `End of turn 1` boundary, then restored the session
  to its original active leaf. Session count stayed stable, and the source
  session still had one compaction root plus the full transcript forest.
- Removed the temporary `session_verify_full_forest_fork` row after the manual
  verification pass.

### Switch Replaces Rewind Command

- Removed `/rewind` and `/tree` from the web slash-command surface. The tree is
  now a picker rendering detail, and same-session history movement is exposed
  only through `/switch`.
- Expanded `/switch` targets to include historical user messages. Selecting one
  uses the existing non-destructive `history.rewind` RPC to move to the
  previous boundary/root and restores that message into the composer. Completed
  turn boundaries and compaction roots still switch the active leaf directly.
- Kept `/switch` idle-only before the picker opens and at target selection time.
  `/fork` remains allowed while the source session is running and still creates
  a new session.
- Verified with `npm run test`, `npm run build`, and a live browser smoke:
  autocomplete no longer showed `/rewind` or `/tree`, `/switch` restored an
  editable user message, running `/switch` surfaced the stop-first error, and
  `/fork` created a child while the source had active work. Temporary
  `simplify-*` smoke sessions were deleted afterward.

### Follow-Up Queueing and Tool Output Bounds

- Investigated session `9a059b37-d14f-4429-b612-5610a4f06ea5`. The follow-up
  failure came from validating `expected_active_leaf_id` before deciding whether
  the input should be queued. That validation is now only applied to idle,
  immediately-materialized inputs; follow-ups for active sessions are future
  queue records and no longer fail just because the live leaf moved.
- Updated the web client to omit `expected_active_leaf_id` while the selected
  session is active. Idle sends still carry the optimistic history check.
- Traced the steer crash to an oversized historical bash tool result. Built-in
  `read` and `bash` now bound returned tool output, and provider requests also
  bound historical tool results so already-existing transcripts cannot overflow
  model context just by being replayed.
- Verified with `npm run test`, `npm run build`,
  `SDKROOT=$(xcrun --show-sdk-path) MACOSX_DEPLOYMENT_TARGET=15.0
  CC=/Library/Developer/CommandLineTools/usr/bin/clang
  CARGO_TARGET_AARCH64_APPLE_DARWIN_LINKER=/Library/Developer/CommandLineTools/usr/bin/clang
  cargo test -p agent-tools -p agent-daemon`, and a websocket smoke that
  queued a follow-up with an intentionally stale leaf while the session was
  running. The temporary smoke session was deleted afterward.
- Fixed steer materialization at the tool-to-model boundary. The core now stops
  at `ReadyToContinue`, the daemon claims a queued steer before continuing the
  model, and the steer is appended as a same-turn `user_message` after the tool
  results. Follow-ups do not use this mid-turn slot. Compaction remains a
  barrier, rewind/switch remain idle-only, and fork does not move queued inputs
  out of the source session.
- Verified by websocket harness smoke: a model requested a sleeping bash tool,
  a follow-up was queued and promoted while the tool was running, and after the
  tool completed the next model action's context contained `tool_result` then
  the promoted steer user message with no `turn_finished` between them. The
  temporary smoke session was deleted afterward.
- Moved stale error-notice handling out of the web UI. The Postgres `events`
  table is now a transient reconnect buffer: missing/null `after_event_id`
  subscribes from the current event head, and once a session publishes
  `session.idle`, the daemon clears that session's event rows. Idle
  configuration, same-session history switching, and fork child creation clear
  the same way after live publication. Durable state still comes from
  `session.get`/`history.tree`, so old `model.error` events are not stored as
  session history or re-shown as fresh notifications.
- Moved fork lineage needed by empty-session pruning out of transient events and
  into child session metadata under `metadata.fork`, so event cleanup does not
  make intentionally empty fork children look accidental.

### Terminal Turn Retry/Continue

- Added a narrow retry/continue primitive for terminal model turns. A crashed
  or interrupted `TurnFinished` can now be resumed through `turn.resume`; the
  daemon finds the original model action checkpoint, sets the session active
  leaf back to that checkpoint, and creates a fresh model action with the same
  turn/action ids.
- Kept the append-only transcript semantics: the old crashed/interrupted
  terminal branch remains in the forest, while the resumed assistant output
  appends as a sibling branch under the original checkpoint. The user message is
  not duplicated, and the active context no longer contains the abandoned
  terminal marker after the resumed turn completes.
- Intentionally limited the first implementation to model-terminal failures and
  interruptions. Interrupted/crashed tool-running turns return `not_resumable`
  until there is an explicit tool rerun policy.
- Added transcript-row Retry/Continue actions plus `/retry` and `/continue`
  slash commands in the web UI. Both call the same `turn.resume` RPC and remain
  idle-only.
- Updated the websocket RPC docs, architecture notes, and README slash-command
  list to include the new lifecycle operation.
- Verified with `cargo test --manifest-path rust/Cargo.toml -p agent-session
  -p agent-store -p agent-daemon`, `npm run build:web --silent`, and a live
  websocket/Postgres harness smoke covering both forced model crash retry and
  model interrupt continue. The smoke verified one user-message row, preserved
  old terminal branches, clean active context, and graceful completion after
  resume; temporary `session_retry_smoke_*` rows were deleted.

### Provider Model Lock And Reasoning Effort

- Verified the current provider APIs before changing the UI contract. OpenAI
  Responses exposes `reasoning.effort` with OpenAI-specific levels from `none`
  through `xhigh`; Anthropic Claude Opus 4.7 exposes `output_config.effort`
  with Claude levels from `low` through `max` and requires adaptive thinking rather than manual
  `budget_tokens`.
- Added `reasoning_effort` to `ProviderConfig`. The web UI writes explicit
  `xhigh` defaults for new OpenAI and Claude sessions, while the serde fallback
  for older stored sessions stays conservative at `medium` so historical Codex
  sessions are not silently replayed at a new effort level. The OpenAI
  serializer accepts only OpenAI effort keys; the Claude serializer accepts only
  Claude effort keys and sends `thinking: { type: "adaptive" }` plus
  `output_config.effort`.
- Removed the `/provider` slash-command path from the web UI. The log header now
  owns model selection and reasoning effort selection. The picker intentionally
  exposes only OpenAI `gpt-5.5` and Claude `claude-opus-4-7` for now.
- Locked `provider.kind` and `provider.model` after the first transcript entry
  at the daemon RPC boundary. Reasoning effort remains configurable even during
  active turns; the daemon updates the active runtime config so the new effort
  applies to subsequent provider requests without pretending that provider/model
  replay state can be migrated mid-session.
- Verified with `cargo check --manifest-path rust/Cargo.toml -p agent-vocab -p
  agent-provider -p agent-daemon` and `npm run build:web --silent`. A full
  `cargo test` attempt reached the linker but failed locally with
  `ld: library not found for -liconv`.

### Responses Overload Debugging

- Investigated new-session `Our servers are currently overloaded` crashes after
  the OpenAI Responses migration. The failing persisted rows were actually
  `codex` provider sessions with no stored `reasoning_effort`, and the daemon
  had no public `OPENAI_API_KEY`; it only had Codex/ChatGPT auth from
  `~/.codex/auth.json`.
- Matched the request policy more closely to the credential path. Public
  OpenAI API-key requests still send `service_tier: "priority"` and
  `prompt_cache_retention: "24h"`, while Codex-auth requests omit both fields.
  This avoids applying public-API cache/service-tier knobs to the ChatGPT Codex
  backend by accident.
- Let OpenAI-configured sessions fall back to Codex auth when no OpenAI API key
  is available, while preserving Codex token refresh on 401s. That keeps the UI
  model picker usable on machines that only have Codex credentials.
- Verified with `cargo check --manifest-path rust/Cargo.toml -p agent-vocab -p
  agent-provider -p agent-daemon`, `npm run build:web --silent`, a rebuilt and
  restarted daemon, and two live websocket smokes. A Codex-auth medium request
  and an OpenAI-configured `xhigh` request using Codex auth both completed
  gracefully; temporary `session_debug_*` rows were deleted afterward.

### Subscription-Only OpenAI Transport

- Removed the daemon's public OpenAI API-key branch. OpenAI-configured sessions
  now always use the ChatGPT/Codex subscription token from `~/.codex/auth.json`
  or `CODEX_ACCESS_TOKEN`; the only OpenAI 401 recovery path is the Codex token
  refresh flow.
- Updated the small `pi-rs openai` CLI path to use the same subscription token
  transport instead of `OPENAI_API_KEY`, and changed its default OpenAI model to
  `gpt-5.5`.
- Renamed the web model label to `GPT-5.5 (ChatGPT)` so the UI does not imply
  public API-key authentication.
- Verified with `cargo check --manifest-path rust/Cargo.toml -p agent-daemon -p
  pi-cli`, `npm run build:web --silent`, a rebuilt/restarted daemon, and a live
  websocket smoke for an `openai:gpt-5.5` `xhigh` session. The smoke completed
  gracefully and the temporary `session_debug_subscription_*` row was deleted.

### Assistant Boundary Design Plan

- Audited the current turn, fork, switch, render, and export assumptions around
  assistant messages. The core already supports multiple model responses inside
  one turn, but the web export and history picker still use pair-shaped labels
  and previews in places.
- Wrote `rust/docs/assistant-boundary-plan.html` with an additive plan: normalize
  provider continuation metadata, derive turn views over transcript paths, keep
  switch stable-boundary-only, keep fork entry-addressable, and make export
  default to final answers or whole turns instead of raw assistant/user pairs.

### Prefix Cache Request Shape

- Made the OpenAI provider subscription-only at the provider boundary as well as
  the daemon boundary. `OpenAiProvider` now always signs requests with the
  ChatGPT/Codex bearer token, optional `ChatGPT-Account-ID`, and the Codex
  residency header; the old plain API-key provider state is gone.
- Changed OpenAI Responses serialization so the stable global system prompt is
  the only `instructions` content. Dynamic daemon context is the first input
  item, followed by transcript replay, so stable instructions/tools can remain
  cacheable across sessions while transcript prefixes can cache within a
  session. The default `prompt_cache_key` is derived from model, stable prompt,
  and sorted tool schema.
- Changed Anthropic Messages serialization to sort tools, mark the final stable
  tool and stable system block with 1-hour `cache_control`, and add one latest
  eligible transcript cache breakpoint. Thinking and redacted thinking replay
  blocks remain preserved but are never used as the cache marker.
- Added provider-neutral usage metrics to `ModelResponse` and persisted them on
  model action results. OpenAI reads `input_tokens_details.cached_tokens`;
  Anthropic reads `cache_creation_input_tokens` and
  `cache_read_input_tokens`. This is intentionally the whole observability
  surface for now; no new inspector panels, logs, or debug commands.
- Updated docs in `README.md`, websocket RPC, architecture, design decisions,
  provider continuity, and `rust/docs/prefix-caching-plan.html` to reflect
  subscription-only OpenAI auth and the implemented cache boundaries.
- Verified with `RUSTFLAGS='-C linker=/Library/Developer/CommandLineTools/usr/bin/cc'
  cargo test --manifest-path rust/Cargo.toml -p agent-provider`,
  `RUSTFLAGS='-C linker=/Library/Developer/CommandLineTools/usr/bin/cc'
  cargo check --manifest-path rust/Cargo.toml -p agent-daemon -p pi-cli`, and
  `RUSTFLAGS='-C linker=/Library/Developer/CommandLineTools/usr/bin/cc'
  cargo test --manifest-path rust/Cargo.toml -p agent-daemon`.

- Mirrored Claude Code-style Anthropic request attribution in `agent-provider` after live 429s from the plain API-key request shape. The Anthropic provider now sends the Claude Code beta/identity headers (`claude-code-20250219`, `User-Agent: claude-cli/...`, `x-app`, session/request ids) and prepends the uncached `x-anthropic-billing-header: cc_version=...; cc_entrypoint=cli;` system block while keeping that provider-specific envelope out of core/session state.
- Verified the attribution fix with a direct Anthropic Messages smoke: the same Claude Code keychain API key that had returned HTTP 429 succeeded with HTTP 200 once the identity/attribution envelope was present.
- Rebuilt and restarted the daemon, then ran a real websocket Anthropic session using `claude-opus-4-7`/`xhigh`. The live turn exercised provider-native Bash, text editor create/view/replace, custom ripgrep, hosted web search, and hosted web fetch. The local tools completed successfully, OpenAI docs web search returned hosted results, web fetch returned an upstream inaccessible-URL result, and the turn finished `Graceful`. Postgres provider replay includes hosted `server_tool_use`, `web_search_tool_result`, and `web_fetch_tool_result` blocks, and model action usage shows prompt cache creation followed by cache reads.

### Prefix Cache Tuning Pass

- Re-audited `agent-provider/src/anthropic.rs` against the Anthropic Messages
  prompt-caching docs (`docs.claude.com/en/docs/build-with-claude/prompt-caching`)
  and `agent-provider/src/openai.rs` against the OpenAI Responses prompt-caching
  guide (`developers.openai.com/api/docs/guides/prompt-caching`). The earlier
  implementation worked but spent breakpoints and TTL premium suboptimally.
- Split the Anthropic `cache_control` helper into `cache_control_1h()` for the
  stable system block only and `cache_control_5m()` (default ephemeral, no
  `ttl`) for the transcript-tail marker. The latest-message breakpoint is
  regenerated each turn, so paying the 1h write premium (2x base input vs
  1.25x for 5m) is pure waste; 5m is the right shelf life for that marker.
- Dropped `mark_last_tool_for_cache`. Anthropic hashes the cumulative prefix
  in `tools -> system -> messages` order, so the stable-system breakpoint
  already covers the entire tools array via the cumulative hash. The
  tools-level marker burned one of 4 breakpoint slots for zero capability,
  and we want that slot back for the deep-history marker below.
- Added a conditional deep-history breakpoint in
  `add_transcript_cache_breakpoints`. When the transcript has more cacheable
  content blocks than `TRANSCRIPT_LOOKBACK_BLOCKS` (18, kept slightly under
  Anthropic's documented ~20 to leave room for in-turn growth), we additionally
  stamp a 5m marker on the cacheable block that's roughly 18 blocks behind the
  tail. Without this, long agentic sessions (each turn producing 6-10
  tool_use/tool_result blocks) silently stop hitting their older cached
  prefix once the gap exceeds the automatic 20-block backward walk.
- Stabilized the Claude Code attribution fingerprint. It used to be derived
  from the first user message, which sat at `system[0]` (before the cache
  breakpoint) and therefore partitioned the cached stable-system prefix
  per-conversation. It now derives from `prompt.stable_prefix`, falling back
  to the first user text only when no stable prefix is configured (e.g.
  compaction calls). Two sessions with the same global system prompt now
  produce identical `system[0]` bytes and can share the cached prefix.
- Hard-coded `thinking: { type: "adaptive" }` with an explanatory comment.
  Anthropic invalidates the message-content cache on any `thinking` change
  (enable/disable or budget), so this parameter must remain a build-time
  constant; reasoning effort already lives in `output_config.effort`, which
  the docs explicitly call out as not affecting messages-level cache.
- Left a `TODO(prefix-cache)` in `responses_body` to set
  `prompt_cache_retention: "24h"` once the Codex subscription transport
  (`chatgpt.com/backend-api/codex`) accepts it. The public Responses API
  documents this parameter, but Codex tracks support in openai/codex#18130
  and currently no-ops or errors when it's sent. 24h retention would close
  the gap with Anthropic's 1h `cache_control` TTL on the stable prefix.
- Updated `rust/docs/prefix-caching-plan.html` so the target request shapes
  and current-state checklist match the new behavior (5m on transcript tail,
  no per-tool marker, conditional deep-history marker, fingerprint stabilized
  off the stable system prompt).
- Refreshed the Anthropic provider tests: existing breakpoint assertions
  switched to `{ type: "ephemeral" }` (no `ttl`) for transcript-tail and
  tool-level no-marker, and added new tests for `stable_system_block_keeps_one_hour_ttl`,
  `short_transcript_uses_only_tail_breakpoint`,
  `long_transcript_adds_deep_history_breakpoint`,
  `attribution_fingerprint_is_stable_across_different_first_user_messages`,
  and `attribution_fingerprint_changes_with_stable_prompt`.
- Verified with `RUSTFLAGS='-C linker=/Library/Developer/CommandLineTools/usr/bin/cc'
  cargo test --manifest-path rust/Cargo.toml -p agent-provider` (25 tests
  passing, including the 6 new ones),
  `RUSTFLAGS='-C linker=/Library/Developer/CommandLineTools/usr/bin/cc'
  cargo check --manifest-path rust/Cargo.toml -p agent-daemon -p pi-cli`,
  and `RUSTFLAGS='-C linker=/Library/Developer/CommandLineTools/usr/bin/cc'
  cargo test --manifest-path rust/Cargo.toml -p agent-daemon` (3 tests).

### Codex CLI Envelope Parity + Per-Session Cache Key

- Audited `~/codex/codex-rs/` (the Codex CLI source) to lift the exact request
  envelope it sends to `chatgpt.com/backend-api/codex/responses`. References:
  `login/src/auth/default_client.rs::default_headers`,
  `model-provider/src/bearer_auth_provider.rs::add_auth_headers`,
  `core/src/client.rs::build_responses_identity_headers` +
  `build_session_headers`, and `codex-api/src/requests/headers.rs`.
- Replaced `OpenAiProvider::add_auth` with `add_codex_headers` that emits the
  full Codex CLI envelope: `originator: codex_cli_rs`, a Codex-shaped
  `User-Agent` (`codex_cli_rs/0.130.0 ({os_type} {version}; {arch})`),
  `x-openai-internal-codex-residency: us`, `Authorization: Bearer`,
  `ChatGPT-Account-ID`, `x-codex-installation-id`, `x-codex-window-id`,
  `x-client-request-id`, and all four spellings of the session/thread id
  (`session_id`, `session-id`, `thread_id`, `thread-id`). Dropped the legacy
  `Accept-Encoding: identity` override since Codex itself doesn't send it.
- Added `os_info` + `uuid` deps to `agent-provider`. `User-Agent` is computed
  once per process via `OnceLock`. The window id is a per-`OpenAiProvider`
  UUID, regenerated when the provider is rebuilt (matches Codex CLI's
  per-process `current_window_id`).
- Plumbed `session_id` through `ModelRequest`. The daemon
  (`provider_runtime.rs`) now passes the pi-relay session id into every
  `run_model` / `run_compaction` call. The OpenAI provider uses that as the
  source of truth for both the `prompt_cache_key` body field and every
  session/thread/request id header. The cache cohort is now unique per
  pi-relay session, matching Codex CLI's `prompt_cache_key =
  thread_id.to_string()` in `core/src/client.rs`. Compaction reuses the
  parent session id with a `:compaction` suffix so headers stay correlated
  for tracing without polluting the main session's cache bucket.
- Added Codex installation-id passthrough. `auth.rs::Credentials::load` now
  reads `~/.codex/installation_id` (the persistent UUID Codex CLI maintains
  at `core/src/installation_id.rs`) and threads it into the provider so the
  `x-codex-installation-id` header matches what a real Codex CLI on the same
  machine would send. Falls back gracefully when absent (pi-cli, tests).
- Deleted the previous content-hash `default_prompt_cache_key` and its
  `StableHasher` helper — the new session-id-as-key strategy supersedes
  them. Added focused tests covering the new ordering: explicit
  `ProviderConfig.prompt_cache.key` override wins; otherwise
  `ModelRequest.session_id` is the key; otherwise the literal session id
  passed at the provider-call boundary is the key.
- Rewrote `codex_auth_adds_account_and_residency_headers` into
  `codex_headers_match_codex_cli_envelope` (full envelope assertion) and
  added `codex_headers_omit_optional_fields_when_absent` to cover the
  no-account-id / no-install-id path. Refreshed all the test sites in
  `agent-provider` to populate the new `session_id` field; tests now run
  through `responses_body` with an explicit test session id.
- Updated `rust/docs/prefix-caching-plan.html` so the OpenAI current-state
  checklist reflects the new envelope (session-id cohort + Codex CLI headers)
  and removes the TODO that lived in `responses_body`.
- Verified with `RUSTFLAGS='-C linker=/Library/Developer/CommandLineTools/usr/bin/cc'
  cargo test --manifest-path rust/Cargo.toml -p agent-provider` (28 tests,
  including 2 new envelope tests and 2 new cache-key tests),
  `cargo test --manifest-path rust/Cargo.toml -p agent-daemon` (3 tests), and
  `cargo check --manifest-path rust/Cargo.toml --workspace`.

### Compaction Interrupt Cancellation

- Fixed stop/`input.interrupt` while a provider-backed compaction is running and
  replaced the daemon's coarse dispatch-task list with a per-action task
  registry for model, tool, and compaction work. Interrupt now aborts registered
  task handles for the session on a best-effort basis instead of waiting for
  provider/tool futures to finish naturally.
- Added `PostgresAgentStore::cancel_unfinished_session_work`, which marks any
  unfinished model/tool/compaction rows for a session `interrupted` and emits
  `session.work_cancelled` without needing an active `AgentSession` runtime.
  This covers the compaction case, where there is no live turn FSM in memory.
- Updated the interrupt RPC to use that store-level cancellation when there is
  no active runtime but unfinished work exists, then drive queued inputs forward
  from the original active leaf. Late compaction completions are guarded by the
  existing unfinished-action compare-and-set and cannot append a summary root.
- Documented interruptable compaction semantics in `docs/websocket-rpc.md`.
- Verified with `cargo test --manifest-path rust/Cargo.toml -p agent-store --lib`
  (5 tests, including 3 env-gated cancellation tests),
  `cargo test --manifest-path rust/Cargo.toml -p agent-daemon` (4 tests), and
  `cargo check --manifest-path rust/Cargo.toml`.
