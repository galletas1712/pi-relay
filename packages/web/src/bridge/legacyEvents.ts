// M11b: bridge contract events → legacy daemon EventFrames, per session.
//
// The legacy daemon pushes immutable, complete transcript entries; the
// in-flight assistant message surfaces as the "Working…" indicator driven by
// activity, NOT as streaming text. This synthesizer mirrors that exactly:
// entries are emitted on COMPLETION boundaries (message end, tool exec end),
// turn boundaries are synthesized (the bridge stream has none), and comms
// arrive whole.
//
// Drift policy: the stream is coherent when deltas nest cleanly
// (start → deltas → end). Attach-mid-turn or a spool gap makes it incoherent;
// the synthesizer then flags drift and the adapter re-serves from getState
// (authoritative) and emits a refresh-triggering frame. Self-healing.

import type { ContractEventEnvelope, CommsMessageData, MessageDeltaData, ToolExecData } from "./types.ts";
import type { AssistantItem, EventFrame, QueuedInput, TranscriptEntry } from "../types.ts";
import { synthesizeTurnFinished, synthesizeTurnStarted, type SynthesisCounters } from "./legacyTranscript.ts";

export interface LiveSeed {
	counters: SynthesisCounters;
	/** Leaf of the entry chain the live entries must parent onto (the rebuilt
	 * branch tip's last synthesized entry id). */
	leafId: string | null;
	/** True when the rebuild left its final turn open (session running). */
	turnOpen: boolean;
	/** Local queue projection (echo of follow-ups queued while running). */
	queued: QueuedInput[];
}

interface OpenAssistant {
	entryId: string;
	text: string;
	toolCalls: { id: string; tool_name: string; args_json: string }[];
	startedAtMs: number;
	/** True when the first observed event for this message was NOT a start —
	 * the prefix streamed before attach; content is incomplete until resync. */
	tainted: boolean;
}

export interface SynthResult {
	frames: EventFrame[];
	/** Set when the stream drifted and the adapter should re-serve getState. */
	drift: boolean;
}

function normalizeToolResultText(resultJson: string | undefined): string {
	if (!resultJson) return "";
	try {
		const obj = JSON.parse(resultJson) as { content?: { type?: string; text?: string }[] };
		if (obj && Array.isArray(obj.content)) {
			const text = obj.content
				.filter((c) => c?.type === "text" && typeof c.text === "string")
				.map((c) => c.text as string)
				.join("\n");
			if (text) return text;
		}
		return resultJson;
	} catch {
		return resultJson;
	}
}

export class LegacySessionSynthesizer {
	private counters: SynthesisCounters;
	private leafId: string | null;
	private turnOpen: boolean;
	private openAssistant: OpenAssistant | null = null;
	private echoQueue: string[] = [];
	private queued: QueuedInput[];
	private queueRevision = 0;
	private sessionRevision = 0;
	private transcriptRevision: number;
	private sawIncoherence = false;
	private liveCounter = 0;
	/** M11c: tool.exec start args by toolCallId — attached to the standalone
	 * tool_result entry when the call's assistant message flushed empty (the
	 * group body renders the call input from it). */
	private pendingToolArgs = new Map<string, string>();
	private compacting = false;

	constructor(
		private readonly sessionId: string,
		seed: LiveSeed,
		revisionBase: number,
	) {
		this.counters = { ...seed.counters };
		this.leafId = seed.leafId;
		this.turnOpen = seed.turnOpen;
		this.queued = [...seed.queued];
		this.transcriptRevision = revisionBase;
	}

	/** Local echo: the bridge stream carries no user-message text, so the
	 * adapter registers the prompt text here (FIFO) before sending. */
	registerEcho(text: string): void {
		this.echoQueue.push(text);
	}

	/** Local queue projection (legacy queued_inputs UI). */
	registerQueued(input: QueuedInput): void {
		this.queued.push(input);
	}

	queueSnapshot(): QueuedInput[] {
		return [...this.queued];
	}

	get currentLeafId(): string | null {
		return this.leafId;
	}

	private nextLiveId(prefix: string): string {
		this.liveCounter += 1;
		return `live-${prefix}-${this.counters.sequence + 1}-${this.liveCounter}`;
	}

	private frame(eventId: number, event: string, data: Record<string, unknown>): EventFrame {
		return { event_id: eventId, event, session_id: this.sessionId, data };
	}

	private appendedFrame(eventId: number, entry: TranscriptEntry, activity: "idle" | "running" | "queued"): EventFrame {
		this.transcriptRevision = Math.max(this.transcriptRevision, eventId);
		return this.frame(eventId, "transcript.appended", {
			entry,
			active_leaf_id: entry.id,
			transcript_revision: this.transcriptRevision,
			session_revision: this.sessionRevision,
			queue_revision: this.queueRevision,
			activity,
			queued_inputs: this.queued,
		});
	}

	/** A transcript.appended frame with no entry makes the legacy cache return
	 * "refresh" — the App refetches (adapter re-serves the re-synced store). */
	refreshFrame(eventId: number): EventFrame {
		return this.frame(eventId, "transcript.appended", {
			transcript_revision: this.transcriptRevision,
			session_revision: this.sessionRevision,
			queue_revision: this.queueRevision,
		});
	}

	private pushEntry(entry: TranscriptEntry): void {
		this.leafId = entry.id;
		this.counters.sequence = entry.sequence ?? this.counters.sequence;
	}

	private closeTurn(atMs: number): TranscriptEntry | null {
		if (!this.turnOpen) return null;
		const { entry, counters } = synthesizeTurnFinished(this.counters.turnId, this.counters, this.leafId, atMs);
		this.counters = counters;
		this.turnOpen = false;
		this.pendingToolArgs.clear(); // aborted/never-ended calls must not leak across turns
		this.pushEntry(entry);
		return entry;
	}

	private flushAssistant(): TranscriptEntry | null {
		const open = this.openAssistant;
		if (!open) return null;
		this.openAssistant = null;
		// M11c (empty-bubble fix): never keep an assistant entry whose text
		// completed empty — it would render as an empty bubble and inflate the
		// turn's agent-message count. Tool-call-only steps are dropped here;
		// their results still arrive as standalone tool_result entries (via
		// tool.exec end) and group in the display model.
		if (open.text.trim().length === 0) return null;
		const items: AssistantItem[] = [
			{ type: "text" as const, text: open.text },
			...open.toolCalls.map((c) => ({ type: "tool_call" as const, ...c })),
		];
		this.counters.sequence += 1;
		const entry: TranscriptEntry = {
			id: open.entryId,
			parent_id: this.leafId,
			timestamp_ms: open.startedAtMs,
			sequence: this.counters.sequence,
			item: { type: "assistant_message", items },
		};
		this.pushEntry(entry);
		return entry;
	}

	/** Feed one bridge contract event; returns legacy frames to dispatch. */
	handle(evt: ContractEventEnvelope): SynthResult {
		const frames: EventFrame[] = [];
		let drift = false;
		const atMs = Date.parse(evt.at) || Date.now();
		const data = evt.data as Record<string, unknown>;

		switch (evt.event) {
			case "message.delta": {
				const d = data as unknown as MessageDeltaData;
				if (d.kind === "start" && d.role === "user") {
					// flush anything pending (gap case), close prior turn, open a new one
					const flushed = this.flushAssistant();
					if (flushed) frames.push(this.appendedFrame(evt.seq, flushed, "running"));
					const closed = this.closeTurn(atMs);
					if (closed) frames.push(this.appendedFrame(evt.seq, closed, "running"));
					const ts = synthesizeTurnStarted(this.counters, this.leafId, atMs);
					this.counters = ts.counters;
					this.turnOpen = true;
					this.pushEntry(ts.entry);
					frames.push(this.appendedFrame(evt.seq, ts.entry, "running"));
					const text = this.echoQueue.shift() ?? "";
					if (this.echoQueue.length === 0 && text === "") this.sawIncoherence = true;
					this.counters.sequence += 1;
					const userEntry: TranscriptEntry = {
						id: this.nextLiveId("user"),
						parent_id: this.leafId,
						timestamp_ms: atMs,
						sequence: this.counters.sequence,
						item: { type: "user_message", content: [{ type: "text", text }] },
					};
					this.pushEntry(userEntry);
					// a queued follow-up got consumed
					if (this.queued.length > 0) {
						this.queued.shift();
						this.queueRevision += 1;
					}
					frames.push(this.appendedFrame(evt.seq, userEntry, "running"));
					frames.push(this.frame(evt.seq, "input.consumed", { activity: "running" }));
					break;
				}
				if (d.kind === "start" && d.role === "assistant") {
					const flushed = this.flushAssistant();
					if (flushed) frames.push(this.appendedFrame(evt.seq, flushed, "running"));
					this.openAssistant = {
						entryId: this.nextLiveId("asst"),
						text: "",
						toolCalls: [],
						startedAtMs: atMs,
						tainted: false,
					};
					break;
				}
				if (d.kind === "text") {
					if (!this.openAssistant) {
						this.openAssistant = {
							entryId: this.nextLiveId("asst"),
							text: "",
							toolCalls: [],
							startedAtMs: atMs,
							tainted: true, // attached mid-message; prefix lost until resync
						};
						this.sawIncoherence = true;
					}
					this.openAssistant.text += d.delta ?? "";
					break;
				}
				if (d.kind === "toolcall") {
					if (!this.openAssistant) {
						this.sawIncoherence = true;
						break;
					}
					try {
						const call = JSON.parse(d.toolCall ?? "{}") as { id?: string; name?: string; arguments?: unknown };
						this.openAssistant.toolCalls.push({
							id: String(call.id ?? ""),
							tool_name: String(call.name ?? "tool"),
							args_json: typeof call.arguments === "string" ? call.arguments : JSON.stringify(call.arguments ?? {}),
						});
					} catch {
						this.sawIncoherence = true;
					}
					break;
				}
				if (d.kind === "end" && d.role === "assistant") {
					const hadOpen = this.openAssistant !== null;
					const flushed = this.flushAssistant();
					if (flushed) frames.push(this.appendedFrame(evt.seq, flushed, "running"));
					else if (!hadOpen) this.sawIncoherence = true; // end without start
					break;
				}
				// thinking deltas + user message_end: dropped (legacy has no
				// thinking part; the user entry was emitted at start).
				break;
			}
			case "tool.exec": {
				const d = data as unknown as ToolExecData;
				if (d.phase === "start") {
					if (d.args) this.pendingToolArgs.set(String(d.toolCallId ?? ""), String(d.args));
					frames.push(this.frame(evt.seq, "tool.started", { activity: "running", tool_name: d.toolName, tool_call_id: d.toolCallId }));
					break;
				}
				if (d.phase === "end") {
					const callId = String(d.toolCallId ?? "");
					const argsJson = this.pendingToolArgs.get(callId);
					this.pendingToolArgs.delete(callId);
					this.counters.sequence += 1;
					const entry: TranscriptEntry = {
						id: `tr-${d.toolCallId}`,
						parent_id: this.leafId,
						timestamp_ms: atMs,
						sequence: this.counters.sequence,
						item: {
							type: "tool_result",
							tool_call_id: d.toolCallId,
							tool_name: d.toolName,
							output: normalizeToolResultText(d.result),
							status: d.isError ? "Error" : "Success",
							...(argsJson !== undefined ? { args_json: argsJson } : {}),
						},
					};
					this.pushEntry(entry);
					frames.push(this.appendedFrame(evt.seq, entry, "running"));
					break;
				}
				break; // updates: legacy renders no partial tool output
			}
			case "comms.message": {
				const d = data as unknown as CommsMessageData;
				this.counters.sequence += 1;
				const entry: TranscriptEntry = {
					id: this.nextLiveId("comms"),
					parent_id: this.leafId,
					timestamp_ms: atMs,
					sequence: this.counters.sequence,
					item: {
						type: "comms_message",
						direction: "in",
						from_name: d.fromName ?? d.fromSessionId ?? "agent",
						to_name: "this session",
						role: d.role ?? null,
						message: d.message ?? "",
						delivery_status: d.deliveryStatus ?? null,
					},
				};
				this.pushEntry(entry);
				frames.push(this.appendedFrame(evt.seq, entry, this.turnOpen ? "running" : "idle"));
				break;
			}
			case "session.state": {
				const state = String(data.state ?? "");
				if (state === "running") {
					if (!this.turnOpen && !this.openAssistant) this.sawIncoherence = true; // attach mid-turn
					frames.push(this.frame(evt.seq, "input.accepted", { activity: "running" }));
					break;
				}
				if (state === "compacting") {
					this.compacting = true;
					frames.push(this.frame(evt.seq, "compaction.requested", { activity: "running" }));
					break;
				}
				if (state === "compacted") {
					// compaction rewrote the file: the branch now differs from the
					// live-synthesized chain — always re-serve from getState.
					this.compacting = false;
					drift = true;
					frames.push(this.frame(evt.seq, "compaction.completed", { activity: "running" }));
					break;
				}
				if (state === "idle") {
					const flushed = this.flushAssistant();
					if (flushed) frames.push(this.appendedFrame(evt.seq, flushed, "running"));
					const closed = this.closeTurn(atMs);
					if (closed) frames.push(this.appendedFrame(evt.seq, closed, "idle"));
					frames.push(this.frame(evt.seq, "session.idle", { activity: "idle" }));
					if (this.sawIncoherence) {
						this.sawIncoherence = false;
						drift = true;
					}
					break;
				}
				if (state === "host_down") {
					frames.push(this.frame(evt.seq, "session.idle", { activity: "idle" }));
					break;
				}
				break;
			}
			case "session.error": {
				frames.push(this.frame(evt.seq, "session.idle", { activity: "idle" }));
				this.sawIncoherence = false;
				drift = true;
				break;
			}
			default:
				break; // session.model, subagent.lifecycle, repl.*: no legacy frame
		}
		return { frames, drift };
	}
}
