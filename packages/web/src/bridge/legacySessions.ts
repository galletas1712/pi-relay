// M11b: per-session client-side store — the adapter's daemon emulation state.
// Holds the synthesized legacy-shaped transcript projection (entries + turn
// cards), the live event synthesizer, and the attach watermark. Pure state
// container; the API adapter (legacyApi.ts) owns all I/O.

import type { SessionState, SessionStateWithTranscript, TranscriptBlock } from "./types.ts";
import type { QueuedInput, TranscriptEntry, TurnCard } from "../types.ts";
import { LegacySessionSynthesizer } from "./legacyEvents.ts";
import { initialCounters, synthesizeEntriesFromBlocks, type SynthesisCounters } from "./legacyTranscript.ts";

export interface SessionStore {
	sessionId: string;
	state: string;
	name: string | null;
	projectId: string | null;
	parentSessionId: string | null;
	cwd: string;
	workspaces: SessionState["workspaces"];
	mcpSelection: SessionState["mcpSelection"];
	model: { provider: string | null; modelId: string | null; thinkingLevel: string | null } | null;
	createdAt: string;
	updatedAt: string;
	// transcript projection
	entries: TranscriptEntry[];
	cards: TurnCard[];
	counters: SynthesisCounters;
	leafId: string | null;
	lastRealEntryId: string | null;
	/** Real id of the active branch's tip block (any kind — repl_cell/custom
	 * included). The App treats active_leaf_id=null as "no transcript" and its
	 * staleness fences conflate null with "unknown", so the adapter reports
	 * leafId ?? branchTipId (the legacy daemon's active leaf is the real branch
	 * tip, never null for a session with entries). */
	branchTipId: string | null;
	turnOpen: boolean;
	/** transcript_revision source: max(headSeq at rebuild, last live seq). */
	revision: number;
	/** Legacy frame-id space: every dispatched EventFrame gets ++frameSeq (the
	 * App drops frames whose event_id is not strictly greater than its
	 * high-water, so multi-frame bridge events need sub-sequenced ids). */
	frameSeq: number;
	headSeq: number;
	synthesizer: LegacySessionSynthesizer | null;
	attached: boolean;
	/** Local queue projection (echo of follow-ups queued while running). */
	queued: QueuedInput[];
}

/** Stable id for a branch-tip block of any kind (tool blocks key on the tool
 * call id, mirroring the synthesized `tr-` entry prefix). */
function blockTipId(block: TranscriptBlock | undefined): string | null {
	if (!block) return null;
	return block.kind === "tool" ? `tr-${block.toolCallId}` : block.id;
}

function runningState(state: string): boolean {
	return state === "running" || state === "compacting" || state === "starting" || state === "respawning";
}

export function storeFromState(state: SessionStateWithTranscript): SessionStore {
	const blocks: TranscriptBlock[] = state.transcript?.blocks ?? [];
	const running = runningState(state.state) || state.live?.isStreaming === true;
	const synth = synthesizeEntriesFromBlocks(state.sessionId, blocks, initialCounters(), { closeFinalTurn: !running });
	const store: SessionStore = {
		sessionId: state.sessionId,
		state: state.state,
		name: state.name,
		projectId: state.projectId ?? null,
		parentSessionId: state.parentSessionId ?? null,
		cwd: state.cwd,
		workspaces: state.workspaces ?? [],
		mcpSelection: state.mcpSelection ?? null,
		model: state.live?.model
			? splitModel(state.live.model, state.live.thinkingLevel ?? null)
			: (state.model ?? null),
		createdAt: state.createdAt,
		updatedAt: state.updatedAt,
		entries: synth.entries,
		cards: synth.cards,
		counters: synth.counters,
		leafId: synth.leafId,
		lastRealEntryId: synth.lastRealEntryId,
		branchTipId: state.branchTipId ?? blockTipId(blocks.at(-1)),
		turnOpen: synth.finalTurnOpen,
		revision: state.headSeq,
		frameSeq: state.headSeq,
		headSeq: state.headSeq,
		synthesizer: null,
		attached: false,
		queued: [],
	};
	store.synthesizer = new LegacySessionSynthesizer(
		state.sessionId,
		{ counters: store.counters, leafId: store.leafId, turnOpen: store.turnOpen, queued: store.queued },
		state.headSeq,
	);
	return store;
}

/** Re-serve from a fresh getState (drift recovery, post-compaction,
 * post-switch/fork, event-gap). Replaces the projection wholesale; the caller
 * emits the refresh-triggering frame so the App refetches. */
export function resyncStore(store: SessionStore, state: SessionStateWithTranscript): void {
	const blocks: TranscriptBlock[] = state.transcript?.blocks ?? [];
	const running = runningState(state.state) || state.live?.isStreaming === true;
	const synth = synthesizeEntriesFromBlocks(store.sessionId, blocks, initialCounters(), { closeFinalTurn: !running });
	store.state = state.state;
	store.name = state.name;
	store.projectId = state.projectId ?? null;
	store.parentSessionId = state.parentSessionId ?? null;
	store.cwd = state.cwd;
	store.workspaces = state.workspaces ?? [];
	store.mcpSelection = state.mcpSelection ?? null;
	store.model = state.live?.model
		? splitModel(state.live.model, state.live.thinkingLevel ?? null)
		: (state.model ?? store.model);
	store.updatedAt = state.updatedAt;
	store.entries = synth.entries;
	store.cards = synth.cards;
	store.counters = synth.counters;
	store.leafId = synth.leafId;
	store.lastRealEntryId = synth.lastRealEntryId;
	store.branchTipId = state.branchTipId ?? blockTipId(blocks.at(-1));
	store.turnOpen = synth.finalTurnOpen;
	store.revision = Math.max(store.revision, state.headSeq);
	store.frameSeq = Math.max(store.frameSeq, state.headSeq);
	store.headSeq = state.headSeq;
	store.queued = [];
	store.synthesizer = new LegacySessionSynthesizer(
		store.sessionId,
		{ counters: store.counters, leafId: store.leafId, turnOpen: store.turnOpen, queued: store.queued },
		state.headSeq,
	);
}

/** The leaf id reported to the App: the synthesized transcript leaf when the
 * branch has visible entries, otherwise the real branch-tip block id (the
 * legacy daemon's active leaf is the real tip — never null mid-history). */
export function reportedLeafId(store: SessionStore): string | null {
	return store.leafId ?? store.branchTipId;
}

export function splitModel(model: string, thinkingLevel: string | null): { provider: string | null; modelId: string | null; thinkingLevel: string | null } {
	const slash = model.indexOf("/");
	if (slash === -1) return { provider: null, modelId: model, thinkingLevel };
	return { provider: model.slice(0, slash), modelId: model.slice(slash + 1), thinkingLevel };
}
