# Rust Rewrite Worklog

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

5. Storage is a trait from the start.
   JSONL is the first backend, but `SessionStore` is deliberately backend
   neutral so a future Postgres backend can implement the same operations
   without touching `agent-session` or the CLI.

6. `agent-orchestrator` was removed.
   After demotion it only contained a live-session registry, so the separate
   crate name was misleading. `SessionRegistry`, `SessionId`, and
   `RegistryError` now live in `agent-session`; they are in-memory process
   state, not durable storage.

7. Existing session semantics are preserved as the runtime foundation.
   `AgentSession` still owns resume, rewind, fork, compaction, open-tail crash
   recovery, queued-input preservation during history edits, and stale-work
   invalidation. The Rust rewrite can deviate from pi-mono where those semantics
   are better for this codebase.

8. Storage snapshots live at the session boundary.
   `agent-store` owns backend-neutral persistence shapes and traits, while
   `agent-session` owns conversion to and from those shapes. This keeps the
   storage crate independent of live session internals but makes persistence
   usable by real sessions.

   Transcript timestamps are `u64` milliseconds. `SystemTime::as_millis()`
   yields `u128`, but JSON does not need 128-bit millisecond values and
   `serde_json` rejects `u128`; `u64` keeps the JSONL backend portable.

9. Provider adapters start as complete-request adapters.
   Streaming can be normalized later inside `agent-provider`; the first Rust
   pass favors a small `ModelProvider::complete` surface that is enough for a
   local session loop.

10. Builtin tools are intentionally unsandboxed primitives.
    `read`, `write`, `edit`, and `bash` are enough for a personal coding loop.
    Permissioning/sandbox policy belongs above `agent-tools` when needed.

11. The CLI is a smoke-test harness, not the product shell.
    `pi-rs` proves the crates compose: session, provider, and tools can drive a
    simple prompt. Durable named sessions and richer UX can be layered on later.

### Implementation Notes

- Added `agent-vocab`, `agent-store`, `agent-provider`, `agent-tools`, and
  `pi-cli` to the Rust workspace.
- Updated `agent-core` to consume structured `UserMessage`s and string tool
  call ids through `agent-vocab`.
- Added session-to-storage conversion:
  `AgentSession::to_stored_session` and `AgentSession::from_stored_session`.
- Added in-memory and JSONL `SessionStore` implementations.
- Added OpenAI and Anthropic provider adapters.
- Added a separate async tool registry with builtin local tools.
- Removed the `agent-orchestrator` crate and moved the independent-session
  registry into `agent-session`.
- Updated the Rust architecture docs and crate READMEs to reflect the new
  target.

### Verification

- `cargo fmt --manifest-path rust/Cargo.toml --all --check` passes.
- `cargo check --manifest-path rust/Cargo.toml --all` passes.
- `cargo test --manifest-path rust/Cargo.toml -p agent-core` passes: 22 tests.
- Full workspace tests pass with the Apple SDK/linker made explicit:
  `SDKROOT=/Library/Developer/CommandLineTools/SDKs/MacOSX.sdk RUSTFLAGS='-C linker=/Library/Developer/CommandLineTools/usr/bin/clang' cargo test --manifest-path rust/Cargo.toml --all`.
  The current suite runs 94 unit tests across the workspace plus doc-test
  harnesses. The default `cc` on this machine is a profile GCC that cannot find
  `libiconv`, so the explicit linker environment is required here.
