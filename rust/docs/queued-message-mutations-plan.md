# Queued Message Mutations Plan

This plan is stacked on top of the session sync redesign.

## Goals

- Add Rust daemon/Postgres support for editing, cancelling, and reordering queued
  follow-up messages.
- Keep steering messages immutable in order and always displayed before
  follow-ups.
- Avoid sparse/gapped ordering. The client sends the complete follow-up order and
  the backend rewrites dense `follow_up_position` values.
- Do not modify TypeScript frontend code in this PR stack.

## Mutability rules

Only rows matching the following predicate are mutable:

```text
priority = follow_up
status = queued
```

Steering rows:

- always sort above follow-ups,
- cannot be reordered,
- are consumed in steering/promote order,
- are not edited/cancelled by the new follow-up mutation RPCs.

## RPCs

### `input.update_queued`

Request:

```json
{
  "session_id": "s1",
  "input_id": "input_123",
  "expected_queue_revision": 4,
  "content": [{ "type": "text", "text": "updated" }]
}
```

If the revision is stale, the backend returns `reason="queue_changed"` with the
canonical queue. If the row is no longer a queued follow-up, it returns
`reason="not_editable"` with the canonical queue. Updating to identical content
is a no-op: the backend returns the current canonical queue without bumping the
queue revision or emitting an event.

### `input.cancel_queued`

Request:

```json
{
  "session_id": "s1",
  "input_id": "input_123",
  "expected_queue_revision": 4
}
```

This marks the row `cancelled`, renumbers remaining queued follow-ups densely,
increments `queue_revision`, and returns the canonical queue.

### `input.reorder_queued_follow_ups`

Request:

```json
{
  "session_id": "s1",
  "expected_queue_revision": 4,
  "input_ids": ["input_c", "input_a", "input_b"]
}
```

The provided ID set must exactly equal the current queued follow-up ID set.
Mismatch returns `reason="queue_changed"` with the canonical queue. Success
rewrites `follow_up_position = 0..n-1`.

Submitting the already-current order is a no-op and does not bump the queue
revision.

## Events

Add:

- `input.updated`
- `input.cancelled`
- `input.reordered`

Each event carries the canonical queue projection and revisions. Existing
`input.queued`, `input.promoted`, `input.consumed`, and `input.accepted` events
also gain the same projection when produced by the new code paths.

## Verification

- RPC parser tests cover the new method names.
- Store-level tests cover edit/cancel/reorder, stale revision rejection,
  steering immutability, and full-list reorder validation.
