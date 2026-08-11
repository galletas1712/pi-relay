# prime-comms — inter-agent messaging extension for unpatched upstream pi

Milestone M2 of the pi-relay migration. Ports prime-agent's (PA) agent
messaging (`agent_message` / `agent_observe` kernel skills) onto the stock
`pi --mode rpc` host (0.84.1) — no fork patches.

## What it provides

In-kernel python modules (installed as `sys.modules` shims over the M1 kernel
bridge `prime_rlm_runtime.host_request`):

- `agent_message.send(target, message)` / `agent_message.list_agents()`
- `agent_observe.get(target)` / `agent_observe.recent(target)` /
  `agent_observe.list()`

Targeting is family-scoped (PA parity): a session may message its parent, its
children (by childId or name), and its siblings. Resolution goes through
prime-rlm's public host API (child registry + parent linkage).

## Delivery

- **To a live child**: `session.sendCustomMessage` with
  `triggerTurn` (idle) / `steer` (streaming), then the handler awaits the
  child settling (`PRIME_COMMS_SETTLE_TIMEOUT_MS`, default 120 s).
- **To the parent**: captured `pi.sendMessage` (triggerTurn/steer) on the
  parent session's extension handle.
- **To an offline target**: the message is recorded in the sender's durable
  outbox (below) and reported as queued.

## Durable outbox (survives host kill -9)

Every send is first appended to
`<sessionDir>/prime/<sessionId>/comms/outbox.jsonl` with status `queued`,
then re-stamped with the delivery outcome. On `session_start`, records whose
last status is `queued` are re-driven in three tiers:

1. target live again → normal delivery,
2. target offline but session file known → `SessionManager.open(file)`
   `appendMessage(...)` (status `persisted`),
3. unresolvable → a recovery notice custom entry
   (`agent_message_outbox_recovery`) is appended to the sender's session.

`PRIME_COMMS_DELIVERY_DELAY_MS` exists purely as a test knob (C2 uses it to
kill the host mid-delivery).

## Dependencies / load order

- **Hard dependency on prime-rlm**: without its host API the extension
  registers nothing (messaging is impossible without the child registry).
- Load order: `prime-rlm` BEFORE `prime-comms`. The seam registration is lazy
  and retried per session, so a mis-ordered load degrades to "no comms"
  rather than crashing.

## Provenance

- `src/protocol.ts` — port of PA `packages/coding-agent/src/core/agent-messages.ts`
  (message shapes, statuses; M2 adds `persisted` / `recovered` statuses and
  carries target session info for post-restart re-drive).
- `src/kernel-shim.ts` — port of PA's agent-message/agent-observe python
  skills, adapted only in the import line (PA: `from rlm import host_request`;
  here: `from prime_rlm_runtime import host_request`).
