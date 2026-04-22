# Root child lifecycle

This Phase 2 sample fixture keeps the replay small on purpose:

1. hydrate a root-only snapshot
2. reserve a pending child spawn
3. upsert the child agent record
4. attach the child to the root
5. drop the pending spawn draft

The command stream uses absolute-ish session/worklog paths while the expected
outputs store only basenames. That exercises path normalization without hiding
meaningful tree-state changes.
