# Query-cache rewrite plan for web session performance PR

## Context

PR #68 currently improves session switching and metadata-operation performance, but its first implementation uses a bespoke cache in `App.tsx`:

- `sessionCacheRef`;
- `snapshot` and `entries` React state;
- `sessions` state plus `sessionsRef`;
- manual stale flags;
- manual selected/list request generations;
- local patch helpers that must update list, snapshot, cache, and refs consistently.

That shape is risky. It makes correctness depend on every RPC handler, websocket event handler, and reconciliation path remembering to mutate several stores in the same way. Adding more guards around that cache would make the code harder to reason about rather than more robust.

The better approach is to replace the bespoke cache with a standard refreshable server-state cache. For this app, use **TanStack Query**.

## Goals

- Keep the PR's user-visible improvements:
  - no stale transcript display after switching sessions;
  - cached/recent session data can render immediately;
  - rename/archive/provider-adjacent changes avoid selected full transcript refreshes;
  - websocket events patch/invalidate state by event type;
  - session list refreshes are coalesced/invalidation-driven.
- Remove the custom session cache from `App.tsx`.
- Make the data ownership model clear:
  - local React state is only UI state;
  - daemon-backed state lives in TanStack Query.
- Make fallback refreshes/invalidation explicit protocol limitations, not scattered defensive checks.
- Keep the current first-PR scope: no daemon protocol changes yet.

## Non-goals

- Do not implement active-branch fetching in this rewrite. Use a query-key shape that can support it later.
- Do not add optimistic archive/rename unless it is trivial and safe. Patch after RPC success first.
- Do not solve server-side metadata clobbering in this client rewrite. That requires daemon metadata patch semantics.
- Do not introduce Suspense as part of this change.
- Do not add React Query devtools unless explicitly wanted.

## Design principles

1. **One owner for server state.** Projects, session lists, session snapshots, transcript entries, tools, and config live in Query Cache.
2. **Query keys isolate races.** A delayed response for session A updates session A's query, not the selected session B UI.
3. **No previous transcript as placeholder.** Selected session queries must not use previous-session data as placeholder data.
4. **Patch through one cache helper path.** RPC success handlers and websocket events should call the same query-cache patch helpers.
5. **Invalidation is the fallback.** If an event payload lacks enough data to patch, invalidate the narrow affected query.
6. **Components render derived state.** Components should not know whether data came from initial fetch, cache, mutation response, or websocket patch.
7. **Fallback reasons should be named.** Event reducer operations should explain why a refresh is required, e.g. incomplete transcript append payload.

## Dependency change

Add TanStack Query to the web package:

```bash
npm install @tanstack/react-query --workspace packages/web
```

This will update `package.json` and `package-lock.json`.

## Provider setup

Update `packages/web/src/main.tsx` to create and provide a `QueryClient`.

Suggested defaults:

```tsx
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";

const queryClient = new QueryClient({
  defaultOptions: {
    queries: {
      staleTime: 15_000,
      gcTime: 10 * 60_000,
      refetchOnWindowFocus: false,
      retry: 1
    }
  }
});

createRoot(document.getElementById("root")!).render(
  <QueryClientProvider client={queryClient}>
    <App />
  </QueryClientProvider>
);
```

## Query keys

Add `packages/web/src/queryKeys.ts`:

```ts
export type EntryScope = "full_tree" | "active_branch";

export const queryKeys = {
  config: ["config"] as const,
  projects: ["projects"] as const,
  tools: (provider: string) => ["tools", provider] as const,
  sessions: (projectId: string | null) => ["sessions", projectId] as const,
  session: (sessionId: string, scope: EntryScope = "full_tree") =>
    ["session", sessionId, scope] as const
};
```

For this PR, normal session display uses `"full_tree"` because the daemon does not yet support active-branch entry scope. Later phases can switch normal display to `"active_branch"` without changing component ownership.

## Server-state queries in `App.tsx`

Use TanStack Query hooks for daemon-backed data.

### Projects

```ts
const projectsQuery = useQuery({
  queryKey: queryKeys.projects,
  queryFn: () => api.listProjects()
});

const projects = projectsQuery.data ?? [];
```

Project selection reconciliation should happen when projects load:

- if the current selected project still exists, keep it;
- otherwise select the first project or `null`;
- if project changes, clear selected session and composer input.

### Sessions

```ts
const sessionsQuery = useQuery({
  queryKey: queryKeys.sessions(selectedProjectId),
  queryFn: () => api.listSessions(100, selectedProjectId),
  enabled: !!selectedProjectId
});

const sessions = sessionsQuery.data ?? [];
```

No manual session-list generation guard is needed. The project ID is in the query key.

### Selected session snapshot and entries

```ts
const selectedSessionQuery = useQuery({
  queryKey: selectedId ? queryKeys.session(selectedId, "full_tree") : ["session", null],
  queryFn: () => api.getSession(selectedId!, { includeEntries: true }),
  enabled: !!selectedId,
  placeholderData: undefined
});
```

Important: do **not** use previous-session placeholder data. A cached result for the same `selectedId` can render immediately, but a different selected session should render loading until that session's own query data is available.

Derived selected state:

```ts
const snapshot = selectedSessionQuery.data ?? null;
const loadedSnapshot = snapshot?.session_id === selectedId ? snapshot : null;
const loadedEntries = loadedSnapshot ? (loadedSnapshot.entries ?? []) : [];
const transcriptLoading = !!selectedId && !loadedSnapshot && selectedSessionQuery.isFetching;
```

Keep the `loadedSnapshot` invariant even though query keys should isolate data. It is a cheap assertion boundary that prevents stale UI if a future query or event path is wrong.

### Config and tools

```ts
const configQuery = useQuery({
  queryKey: queryKeys.config,
  queryFn: () => api.getConfig()
});

const toolsQuery = useQuery({
  queryKey: queryKeys.tools(activeProvider.kind),
  queryFn: () => api.listTools(activeProvider.kind),
  enabled: connection === "open"
});
```

Use defaults while loading:

```ts
const config = configQuery.data ?? { system_prompt: null };
const tools = toolsQuery.data ?? [];
```

## State to keep local

Keep local React state for UI-only concerns:

- `selectedProjectId`;
- `selectedId`;
- dialog state;
- composer handle/drafts;
- `query` sidebar filter;
- `showArchived`;
- `rightOpen`;
- `sending`, `stopping`, `resumingTurnId`;
- notices.

Keep refs for websocket subscription bookkeeping, not rendering data:

- `subscribedEventSessionIds`;
- `lastEventIds`;
- `selectedRef` and `selectedProjectRef` if needed by websocket callbacks.

Remove from `App.tsx`:

- `sessionCacheRef`;
- `CachedSession`;
- custom cache stale flags;
- `writeCachedSession`;
- `patchCachedSession`;
- `markCachedSessionStale`;
- `deleteCachedSession`;
- `trimSessionCache`;
- `oldestCachedSessionId`;
- `totalCachedEntryCount`;
- `selectedRequestGeneration`;
- `sessionListGeneration`;
- manual `snapshot` and `entries` state;
- manual `projects`, `sessions`, `config`, `tools` state.

## Query-cache patch helpers

Add `packages/web/src/sessionQueryCache.ts`.

Suggested helpers:

```ts
import type { QueryClient } from "@tanstack/react-query";
import { queryKeys } from "./queryKeys.ts";
import type { ProviderConfig, SessionSnapshot, SessionSummary } from "./types.ts";

export function patchSessionList(
  queryClient: QueryClient,
  projectId: string | null,
  sessionId: string,
  patcher: (session: SessionSummary) => SessionSummary
) {
  if (!projectId) return;
  queryClient.setQueryData<SessionSummary[]>(queryKeys.sessions(projectId), (current) => {
    if (!current) return current;
    let changed = false;
    const next = current.map((session) => {
      if (session.session_id !== sessionId) return session;
      changed = true;
      return patcher(session);
    });
    return changed ? next : current;
  });
}

export function patchSessionSnapshot(
  queryClient: QueryClient,
  sessionId: string,
  patcher: (snapshot: SessionSnapshot) => SessionSnapshot
) {
  queryClient.setQueryData<SessionSnapshot>(queryKeys.session(sessionId, "full_tree"), (current) =>
    current ? patcher(current) : current
  );
}
```

Metadata helpers:

```ts
export function mergeMetadata(
  metadata: Record<string, unknown>,
  patch: Record<string, unknown>,
  remove: string[] = []
): Record<string, unknown> {
  const next = { ...metadata, ...patch };
  for (const key of remove) delete next[key];
  return next;
}
```

Patch both list and snapshot from one public helper:

```ts
export function patchSessionMetadataEverywhere(
  queryClient: QueryClient,
  projectId: string | null,
  sessionId: string,
  patch: Record<string, unknown>,
  remove: string[] = []
) {
  patchSessionList(queryClient, projectId, sessionId, (session) => ({
    ...session,
    metadata: mergeMetadata(session.metadata, patch, remove)
  }));
  patchSessionSnapshot(queryClient, sessionId, (snapshot) => ({
    ...snapshot,
    metadata: mergeMetadata(snapshot.metadata, patch, remove)
  }));
}
```

Provider/activity helpers should follow the same pattern.

## Mutations

The first implementation can keep plain async handlers. `useMutation` is optional but recommended if it makes status/error handling clearer.

### Rename

After RPC success:

```ts
await api.renameSession(sessionId, title);
patchSessionMetadataEverywhere(queryClient, selectedProjectId, sessionId, { title });
queryClient.invalidateQueries({ queryKey: queryKeys.sessions(selectedProjectId) });
```

No selected `session.get(include_entries: true)`.

### Archive/unarchive

After RPC success:

```ts
await api.configureSession({ sessionId, provider, metadata });
patchSessionMetadataEverywhere(
  queryClient,
  selectedProjectId,
  sessionId,
  archived ? { archived: true } : {},
  archived ? [] : ["archived"]
);
queryClient.invalidateQueries({ queryKey: queryKeys.sessions(selectedProjectId) });
```

No selected transcript refresh.

Caveat: this still uses full metadata replacement at the daemon boundary. Concurrent metadata clobbering remains possible until the daemon supports metadata patch semantics.

### Provider/reasoning effort

After RPC success:

```ts
await api.configureSession({ sessionId, provider });
patchSessionProviderEverywhere(queryClient, selectedProjectId, sessionId, provider);
queryClient.invalidateQueries({ queryKey: queryKeys.sessions(selectedProjectId) });
```

No selected transcript refresh.

### Delete

After RPC success:

```ts
queryClient.removeQueries({ queryKey: queryKeys.session(sessionId, "full_tree") });
queryClient.invalidateQueries({ queryKey: queryKeys.sessions(selectedProjectId) });
```

If the deleted session is selected, clear `selectedId`.

### Start/fork/switch/resume/stop/promote

These can remain conservative:

- operations that genuinely mutate transcript/history may invalidate selected session query;
- operations that only need list reconciliation invalidate the list query;
- full refreshes should be expressed as `queryClient.invalidateQueries(...)`, not custom cache mutation.

## Websocket events

Keep the reducer idea from PR #68, but make it the single source of event-to-cache semantics.

### Event reducer

`packages/web/src/sessionEvents.ts` should export typed operations:

```ts
export type SessionPatchOperation =
  | { type: "metadata"; sessionId: string; patch: Record<string, unknown>; remove: string[] }
  | { type: "provider"; sessionId: string; provider: ProviderConfig }
  | { type: "activity"; sessionId: string; activity: Activity }
  | { type: "queued_inputs"; sessionId: string; event: EventFrame }
  | { type: "invalidate_session"; sessionId: string; reason: string }
  | { type: "invalidate_list"; reason: string };
```

Fallbacks should be explicit protocol limitations:

- `input.queued` -> invalidate selected session because current event payload does not include a complete `QueuedInput` record with status/timestamps.
- `transcript.appended` -> invalidate selected session because current event payload lacks full `TranscriptEntry` data.
- `history.rewound` / `history.compacted` -> invalidate selected session because branch structure changed.
- `session.configured` -> patch metadata/provider when payload has enough data, invalidate list, but do not invalidate selected transcript.

### Apply operations to Query Cache

Add an operation applier, either in `sessionQueryCache.ts` or `App.tsx` initially:

```ts
function applySessionQueryOperation(operation: SessionPatchOperation) {
  switch (operation.type) {
    case "metadata":
      patchSessionMetadataEverywhere(queryClient, selectedProjectId, operation.sessionId, operation.patch, operation.remove);
      return;
    case "provider":
      patchSessionProviderEverywhere(queryClient, selectedProjectId, operation.sessionId, operation.provider);
      return;
    case "activity":
      patchSessionActivityEverywhere(queryClient, selectedProjectId, operation.sessionId, operation.activity);
      return;
    case "queued_inputs":
      patchQueuedInputsInSnapshot(queryClient, operation.event);
      return;
    case "invalidate_session":
      queryClient.invalidateQueries({ queryKey: queryKeys.session(operation.sessionId, "full_tree") });
      return;
    case "invalidate_list":
      queryClient.invalidateQueries({ queryKey: queryKeys.sessions(selectedProjectId) });
      return;
  }
}
```

Selected-session invalidation does not need custom selected request generation. Query keys isolate responses.

## Subscription bookkeeping

Continue using `events.subscribe`/`events.unsubscribe` with refs:

```ts
const subscribedEventSessionIds = useRef(new Set<string>());
const lastEventIds = useRef(new Map<string, number>());
```

Desired subscription IDs come from query data:

```ts
const desiredSessionIds = new Set(sessions.map((session) => session.session_id));
if (selectedId) desiredSessionIds.add(selectedId);
```

On event:

```ts
lastEventIds.current.set(
  event.session_id,
  Math.max(lastEventIds.current.get(event.session_id) ?? 0, event.event_id)
);
```

Project filtering should avoid a mirrored `sessionsRef` if possible. Prefer reading current query data:

```ts
const currentSessions = queryClient.getQueryData<SessionSummary[]>(queryKeys.sessions(selectedProjectId));
const eventSession = currentSessions?.find((session) => session.session_id === event.session_id);
if (eventSession?.project_id && eventSession.project_id !== selectedProjectId) return;
```

If no session summary is found, do not over-filter. Let operation invalidation be narrow by session ID/list key.

## Loading and stale-display behavior

Keep `MessageList` changes from PR #68:

- accepts `loadingSession`;
- checks `entriesSessionId === sessionId`;
- derives display nodes from `effectiveEntries`, not stale entries;
- renders loading when selected and loaded sessions differ.

This remains useful even with query keys.

## Tests

Keep existing tests from PR #68:

- stale transcript guard in `MessageList`.

Keep/add reducer tests:

- `session.configured` patches metadata and invalidates list without invalidating selected transcript;
- `input.queued` invalidates selected session with reason `queued input payload is not a complete queued-input snapshot`;
- `input.promoted`/`input.consumed` patches queued inputs and invalidates list without selected transcript invalidation;
- `transcript.appended` invalidates selected session with reason `transcript append event lacks full entry data`.

Add cache-operation tests if practical:

- metadata operation patches session list and session snapshot query data;
- provider operation patches both list and snapshot;
- activity operation patches both list and snapshot;
- invalidate-session operation targets only that session query key;
- selected session query does not render previous session data.

## Migration checklist for PR #68

1. Add `@tanstack/react-query` to `packages/web`.
2. Add `QueryClientProvider` in `main.tsx`.
3. Add `queryKeys.ts`.
4. Add/adjust `sessionEvents.ts` reducer operations for query invalidation semantics.
5. Add `sessionQueryCache.ts` patch/invalidate helpers.
6. Replace `projects`, `sessions`, `snapshot`, `entries`, `config`, and `tools` state in `App.tsx` with queries.
7. Remove custom cache and generation code from `App.tsx`.
8. Convert rename/archive/provider handlers to RPC success -> query-cache patch -> query invalidation.
9. Convert websocket event handler to reducer -> query-cache operation applier.
10. Keep `loadedSnapshot`, `loadedEntries`, and `transcriptLoading` derivation.
11. Keep `MessageList` stale-display guard.
12. Run:

```bash
npm run build:web
npm run test --workspace packages/web
```

13. Update PR description to state that the first version's bespoke cache was replaced with TanStack Query server-state caching.

## Expected result

After this rewrite, PR #68 should have a simpler data model:

- React local state describes UI choices.
- TanStack Query owns daemon-backed data and refresh behavior.
- Websocket/RPC updates patch or invalidate query data through one helper path.
- Fallbacks are explicit reducer operations tied to known protocol gaps.
- There is no custom `sessionCacheRef` with manual stale flags and multi-store synchronization.
