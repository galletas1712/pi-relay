// React bindings for the bridge data layer. Follows the SPA's existing
// architecture: TanStack Query for request/response server state (session
// list, subagent tree — with the same 2 s list-refetch safety net the legacy
// app uses), and a useSyncExternalStore bridge over the event-sourced
// BridgeSessionStore for the live transcript projection.
import { QueryClient, useQuery, useQueryClient, type QueryClient as QueryClientType } from "@tanstack/react-query";
import {
	createContext,
	useContext,
	useEffect,
	useMemo,
	useRef,
	useState,
	useSyncExternalStore,
	type ReactNode,
} from "react";
import { BridgeClient, type BridgeConnectionStatus } from "./client.ts";
import { BridgeSessionStore } from "./sessionStore.ts";
import type { SessionProjection } from "./eventStore.ts";
import { emptySessionProjection } from "./eventStore.ts";
import { ReplStore, emptyReplProjection, type ReplProjection } from "./replStore.ts";

export interface BridgeContextValue {
	client: BridgeClient;
	store: BridgeSessionStore;
	/** M9: repl console projections (repl.* events, per session). */
	replStore: ReplStore;
}

const BridgeContext = createContext<BridgeContextValue | null>(null);

/** Same-origin WS path the vite dev proxy forwards to the bridge, injecting
 * the upgrade headers a browser cannot set (Authorization, allowlisted
 * Origin). Direct-URL override via VITE_BRIDGE_WS_URL for non-proxied setups. */
export function bridgeWebSocketUrl(): string {
	const override = import.meta.env?.VITE_BRIDGE_WS_URL;
	if (typeof override === "string" && override !== "") return override;
	const proto = window.location.protocol === "https:" ? "wss:" : "ws:";
	return `${proto}//${window.location.host}/__bridge-ws`;
}

export function createBridgeQueryClient(): QueryClientType {
	return new QueryClient({
		defaultOptions: {
			queries: {
				retry: 1,
				staleTime: 500,
				refetchOnWindowFocus: true,
			},
		},
	});
}

export function BridgeProvider({ client, children }: { client?: BridgeClient; children: ReactNode }) {
	const value = useMemo<BridgeContextValue>(() => {
		const c = client ?? new BridgeClient(bridgeWebSocketUrl());
		return { client: c, store: new BridgeSessionStore(c), replStore: new ReplStore(c) };
	}, [client]);
	useEffect(() => {
		void value.client.connect().catch(() => {});
		return () => {
			value.store.dispose();
			value.replStore.dispose();
			value.client.close();
		};
	}, [value]);
	return <BridgeContext.Provider value={value}>{children}</BridgeContext.Provider>;
}

export function useBridge(): BridgeContextValue {
	const ctx = useContext(BridgeContext);
	if (!ctx) throw new Error("useBridge outside BridgeProvider");
	return ctx;
}

export function useConnectionStatus(): BridgeConnectionStatus {
	const { client } = useBridge();
	const [status, setStatus] = useState<BridgeConnectionStatus>("connecting");
	useEffect(() => client.onStatus(setStatus), [client]);
	return status;
}

export const bridgeQueryKeys = {
	sessionList: ["bridge", "sessions"] as const,
	projectList: ["bridge", "projects"] as const,
	sessionState: (id: string) => ["bridge", "session", id, "state"] as const,
	subagentTree: (id: string) => ["bridge", "session", id, "subagents"] as const,
	modelsList: ["bridge", "models"] as const,
	subagentTranscript: (id: string, childId: string) => ["bridge", "session", id, "subagents", childId, "transcript"] as const,
	commsList: (id: string) => ["bridge", "session", id, "comms"] as const,
};

const SESSION_LIST_REFETCH_MS = 2_000;

export function useSessionList() {
	const { client } = useBridge();
	return useQuery({
		queryKey: bridgeQueryKeys.sessionList,
		queryFn: async () => (await client.listSessions()).sessions,
		refetchInterval: SESSION_LIST_REFETCH_MS,
	});
}

/** M8 (G1): project catalog for sidebar grouping (same freshness discipline). */
export function useProjectList() {
	const { client } = useBridge();
	return useQuery({
		queryKey: bridgeQueryKeys.projectList,
		queryFn: async () => (await client.listProjects()).projects,
		refetchInterval: SESSION_LIST_REFETCH_MS,
	});
}

/** Attach + project the live event stream for the selected session. */
export function useSessionProjection(sessionId: string | null): SessionProjection | null {
	const { store } = useBridge();
	const subscribe = useMemo(() => store.subscribe, [store]);
	const snapshot = useSyncExternalStore(
		subscribe,
		() => (sessionId ? (store.getSnapshot(sessionId) ?? EMPTY) : null),
		() => null,
	);
	useEffect(() => {
		if (!sessionId) return;
		void store.attach(sessionId).catch(() => {});
		return () => {
			void store.detach(sessionId).catch(() => {});
		};
	}, [store, sessionId]);
	return sessionId ? (snapshot === EMPTY ? emptySessionProjection(sessionId) : snapshot) : null;
}

const EMPTY: SessionProjection | null = null;

/** M9: repl console projection for the selected session (repl.* events). */
export function useReplProjection(sessionId: string | null): ReplProjection | null {
	const { replStore } = useBridge();
	const subscribe = useMemo(() => replStore.subscribe, [replStore]);
	const snapshot = useSyncExternalStore(
		subscribe,
		() => (sessionId ? (replStore.getSnapshot(sessionId) ?? EMPTY_REPL) : null),
		() => null,
	);
	return sessionId ? (snapshot === EMPTY_REPL ? emptyReplProjection(sessionId) : snapshot) : null;
}

const EMPTY_REPL: ReplProjection | null = null;

/** Subagent tree: event-driven invalidation on subagent.lifecycle plus the
 * request/response tree for the durable (session-file) view. */
export function useSubagentTree(sessionId: string | null, projection: SessionProjection | null) {
	const { client, store } = useBridge();
	const queryClient = useQueryClient();
	const lifecycleSeq = projection?.subagents.reduce((max, s) => Math.max(max, s.lastSeq), 0) ?? 0;
	const query = useQuery({
		queryKey: sessionId ? bridgeQueryKeys.subagentTree(sessionId) : ["bridge", "subagents", "none"],
		queryFn: async () => (sessionId ? await client.subagentTree(sessionId) : { sessionId: "", children: [] }),
		enabled: sessionId !== null,
	});
	const lastSeqRef = useRef(lifecycleSeq);
	useEffect(() => {
		if (!sessionId || lifecycleSeq === lastSeqRef.current) return;
		lastSeqRef.current = lifecycleSeq;
		void queryClient.invalidateQueries({ queryKey: bridgeQueryKeys.subagentTree(sessionId) });
	}, [sessionId, lifecycleSeq, queryClient, store]);
	return query;
}

/** M11a: model catalog (bridge caches 15 s; refetch is cheap). */
export function useModelsList() {
	const { client } = useBridge();
	return useQuery({
		queryKey: bridgeQueryKeys.modelsList,
		queryFn: async () => await client.listModels(),
		staleTime: 10_000,
	});
}

/** M11a: child transcript + repl cells, load-on-open. Call refetch() for the
 * explicit refresh (no polling — child state changes rarely and the spool is
 * not live-tailed in v0.2). */
export function useSubagentTranscript(sessionId: string | null, childId: string | null) {
	const { client } = useBridge();
	return useQuery({
		queryKey:
			sessionId && childId
				? bridgeQueryKeys.subagentTranscript(sessionId, childId)
				: ["bridge", "subagent-transcript", "none"],
		queryFn: async () => await client.subagentTranscript(sessionId!, childId!),
		enabled: sessionId !== null && childId !== null,
	});
}

/** M11a: comms history for the selected session. Live annotations flow via
 * comms.message events; this covers history beyond the spool window and the
 * outbound-only view. */
export function useCommsList(sessionId: string | null, projection: SessionProjection | null) {
	const { client } = useBridge();
	const lastCommsSeq = useMemo(() => {
		let max = 0;
		for (const block of projection?.blocks ?? []) {
			if (block.kind === "comms" && block.fromSeq && block.fromSeq > max) max = block.fromSeq;
		}
		return max;
	}, [projection?.blocks]);
	const query = useQuery({
		queryKey: sessionId ? bridgeQueryKeys.commsList(sessionId) : ["bridge", "comms", "none"],
		queryFn: async () => await client.commsList(sessionId!),
		enabled: sessionId !== null,
	});
	const queryClient = useQueryClient();
	const lastSeqRef = useRef(lastCommsSeq);
	useEffect(() => {
		if (!sessionId || lastCommsSeq === lastSeqRef.current) return;
		lastSeqRef.current = lastCommsSeq;
		void queryClient.invalidateQueries({ queryKey: bridgeQueryKeys.commsList(sessionId) });
	}, [sessionId, lastCommsSeq, queryClient]);
	return query;
}
