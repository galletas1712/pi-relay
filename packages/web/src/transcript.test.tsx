import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it, vi } from "vitest";
import {
	assistantRenderParts,
	captureScrollPosition,
	editToolPreview,
	formatElapsed,
	isScrolledAtBottom,
	loadTranscriptScrollPositions,
	MessageList,
	restoreScrollPosition,
	runningTurnClockAnchor,
	runningTurnStartMs,
	saveTranscriptScrollPositions,
	stableWorkingElapsedMs,
	TRANSCRIPT_SCROLL_STORAGE_KEY,
	type TranscriptScrollStorage,
	ToolOutput,
} from "./transcript.tsx";
import type { AssistantItem, ProviderReplayItem, TranscriptEntry } from "./types.ts";

describe("assistantRenderParts", () => {
	it("uses local replay display metadata even when no hosted tools are present", () => {
		const parts = assistantRenderParts(
			[toolCall("call_1", "Edit")],
			[
				replay(
					"claude",
					{ type: "tool_use", id: "call_1", name: "Edit" },
					{ kind: "local_tool", pretty_name: "Edit", input_summary: "view tmp/file.txt" }
				)
			]
		);

		expect(parts).toMatchObject([
			{
				type: "tool_call",
				display: {
					pretty_name: "Edit",
					input_summary: "view tmp/file.txt"
				}
			}
		]);
	});

	it("renders canonical OpenAI Edit as an edit diff preview", () => {
		const preview = editToolPreview("Edit", {
			input: "*** Begin Patch\n*** Add File: tmp/example.txt\n+hello\n*** End Patch\n"
		});

		expect(preview).toMatchObject({
			header: "Edited example.txt +1",
			action: "Edited",
			file: "tmp/example.txt",
			additions: 1,
			deletions: 0,
			kind: "diff",
			rows: [{ kind: "add", text: "hello" }]
		});
	});

	it("renders canonical Claude Edit as an edit diff preview", () => {
		const preview = editToolPreview("Edit", {
			command: "str_replace",
			path: "/repo/tmp/example.txt",
			old_str: "alpha\n",
			new_str: "beta\n"
		});

		expect(preview).toMatchObject({
			header: "Edited example.txt +1 -1",
			action: "Edited",
			file: "/repo/tmp/example.txt",
			additions: 1,
			deletions: 1,
			kind: "diff",
			rows: [
				{ kind: "remove", text: "alpha" },
				{ kind: "add", text: "beta" }
			]
		});
	});
});

describe("MessageList compaction display", () => {
	it("keeps pre-compaction entries visible by default", () => {
		const html = renderToStaticMarkup(
			<MessageList
				entries={[
					turnStartedEntry("start", 1, 1),
					userEntryWithParent("user", "start", "before compaction"),
					turnFinishedEntry("finish", "user", 1, "Graceful"),
					compactionSummaryEntry("compact", null, 1, 2, null, "finish"),
				]}
				activeLeafId="compact"
				isRunning={false}
				serverTimeMs={null}
				hasSession
				sessionId="session_a"
				entriesSessionId="session_a"
			/>
		);

		expect(html).toContain("before compaction");
		expect(html).toContain("Context compacted through turn 1");
		expect(html).toContain("Hide prior");
		expect(html).not.toContain("prior entries hidden");
	});
});

function toolCall(id: string, toolName: string): AssistantItem {
	return { type: "tool_call", id, tool_name: toolName, args_json: "{}" };
}

function replay(provider: ProviderReplayItem["provider"], raw: unknown, display: NonNullable<ProviderReplayItem["display"]>): ProviderReplayItem {
	return {
		provider,
		raw_json: JSON.stringify(raw),
		display
	};
}

describe("isScrolledAtBottom", () => {
	it("treats the exact bottom and sub-pixel distance as pinned", () => {
		expect(isScrolledAtBottom({ scrollHeight: 1000, scrollTop: 600, clientHeight: 400 })).toBe(true);
		expect(isScrolledAtBottom({ scrollHeight: 1000, scrollTop: 599.25, clientHeight: 400 })).toBe(true);
		expect(isScrolledAtBottom({ scrollHeight: 1000, scrollTop: 598.9, clientHeight: 400 })).toBe(false);
	});

	it("treats overscroll past the bottom as pinned", () => {
		expect(isScrolledAtBottom({ scrollHeight: 1000, scrollTop: 601, clientHeight: 400 })).toBe(true);
	});
});

describe("scroll position snapshots", () => {
	it("restores an unpinned scroll offset", () => {
		const node = { scrollHeight: 1000, scrollTop: 600, clientHeight: 400 };
		const position = captureScrollPosition({ ...node, scrollTop: 250 });

		const sticky = restoreScrollPosition(node, position);

		expect(node.scrollTop).toBe(250);
		expect(sticky).toBe(false);
	});

	it("restores sticky-bottom as the current bottom", () => {
		const node = { scrollHeight: 1400, scrollTop: 0, clientHeight: 400 };

		const sticky = restoreScrollPosition(node, { scrollTop: 600, sticky: true });

		expect(node.scrollTop).toBe(1000);
		expect(sticky).toBe(true);
	});

	it("persists transcript scroll positions by session key", () => {
		const storage = memoryStorage();
		const positions = new Map([
			["session_a", { scrollTop: 250, sticky: false }],
			["session_b", { scrollTop: 900, sticky: true }],
		]);

		saveTranscriptScrollPositions(positions, storage);

		expect(loadTranscriptScrollPositions(storage)).toEqual(positions);
	});

	it("clears persisted transcript scroll positions when none remain", () => {
		const storage = memoryStorage();

		saveTranscriptScrollPositions(new Map(), storage);

		expect(storage.getItem(TRANSCRIPT_SCROLL_STORAGE_KEY)).toBeNull();
	});
});

describe("MessageList session loading guard", () => {
	it("shows a loading state instead of stale entries when entries belong to another session", () => {
		const html = renderToStaticMarkup(
			<MessageList
				entries={[userEntry("entry_1", "stale transcript text")]}
				activeLeafId="entry_1"
				isRunning={false}
				serverTimeMs={null}
				hasSession
				sessionId="session_b"
				entriesSessionId="session_a"
			/>
		);

		expect(html).toContain("Loading session");
		expect(html).not.toContain("stale transcript text");
	});
});

describe("MessageList markdown code rendering", () => {
	it("renders inline code and syntax-highlighted code blocks", () => {
		const html = renderToStaticMarkup(
			<MessageList
				entries={[assistantEntry("assistant", null, "Inline `value`.\n\n```js\nconst value = 1;\n```")]}
				activeLeafId="assistant"
				isRunning={false}
				serverTimeMs={null}
				hasSession
				sessionId="session_a"
				entriesSessionId="session_a"
			/>
		);

		expect(html).toContain("<code>value</code>");
		expect(html).not.toContain("code-language-label");
		expect(html).toContain("hljs-keyword");
	});

	it("renders ```mermaid fences with the diagram placeholder instead of a raw code block", () => {
		const diagram = "flowchart LR\n  A --> B";
		const html = renderToStaticMarkup(
			<MessageList
				entries={[assistantEntry("assistant", null, "Here is a diagram:\n\n```mermaid\n" + diagram + "\n```")]}
				activeLeafId="assistant"
				isRunning={false}
				serverTimeMs={null}
				hasSession
				sessionId="session_a"
				entriesSessionId="session_a"
			/>
		);

		// SSR renders before the client effect runs, so we expect the source
		// fallback (not the syntax-highlighted .hljs version) wrapped in a
		// `.mermaid-source` <pre>, and no <code class="hljs ..."> wrapper.
		expect(html).toContain("mermaid-source");
		expect(html).toContain("flowchart LR");
		expect(html).not.toContain("hljs language-mermaid");
	});
});

describe("ToolOutput", () => {
	it("does not truncate long output text in markup", () => {
		const output = Array.from({ length: 60 }, (_, index) => `line ${index + 1}`).join("\n");
		const html = renderToStaticMarkup(<ToolOutput result={{ type: "tool_result", tool_call_id: "call_1", tool_name: "Bash", status: "Success", output }} />);

		expect(html).toContain("line 60");
		expect(html).not.toContain("\\n...");
	});
});

describe("MessageList tool use cards", () => {
	it("renders a single tool directly instead of a grouped Used 1 tool header", () => {
		const bashTool = { type: "tool_call" as const, id: "call_1", tool_name: "Bash", args_json: "{\"command\":\"ls\"}" };
		const html = renderToStaticMarkup(
			<MessageList
				entries={[
					turnStartedEntry("start", 1, 1),
					userEntryWithParent("user", "start", "inspect"),
					assistantToolEntry("assistant", "user", [bashTool]),
					toolResultEntry("result", "assistant", "call_1", "Bash", "ok"),
					turnFinishedEntry("finish", "result", 1, "Graceful")
				]}
				activeLeafId="finish"
				isRunning={false}
				serverTimeMs={null}
				hasSession
				sessionId="session_a"
				entriesSessionId="session_a"
			/>
		);

		expect(html).toContain("single-tool");
		expect(html).toContain("Bash: ls");
		expect(html).not.toContain("Used 1 tool");
	});
});

describe("MessageList Working indicator", () => {
	it("uses the persisted turn_started timestamp for the running turn", () => {
		expect(runningTurnStartMs([turnStartedEntry("entry_turn", 1, 1234)])).toBe(1234);
	});

	it("uses a mid-turn compaction summary turn start when the turn start is no longer on the active branch", () => {
		expect(runningTurnStartMs([compactionSummaryEntry("compact", null, 1, 5_000, 1234)])).toBe(1234);
	});

	it("does not walk past a finished turn", () => {
		expect(runningTurnStartMs([
			turnStartedEntry("start", 1, 1000),
			turnFinishedEntry("finish", "start", 1, "Graceful"),
		])).toBeNull();
	});

	it("anchors elapsed time to the server clock for cross-machine display", () => {
		const nowSpy = vi.spyOn(performance, "now").mockReturnValue(12_000);
		try {
			expect(runningTurnClockAnchor([turnStartedEntry("entry_turn", 1, 1_000)], 10_000)).toEqual({
				startMs: 1_000,
				serverAnchorMs: 10_000,
				clientAnchorMs: 12_000,
			});
		} finally {
			nowSpy.mockRestore();
		}
	});

	it("does not synthesize a local clock when the server time is missing", () => {
		expect(runningTurnClockAnchor([turnStartedEntry("entry_turn", 1, 1_000)], null)).toBeNull();
	});

	it("keeps a stable working clock anchor across transcript updates", () => {
		const nowSpy = vi.spyOn(performance, "now");
		try {
			nowSpy.mockReturnValue(1_000);
			const initial = stableWorkingElapsedMs(null, 1_000, 10_000);
			nowSpy.mockReturnValue(2_000);
			const updated = stableWorkingElapsedMs(initial.clock, 1_000, 10_000);

			expect(initial.elapsedMs).toBe(9_000);
			expect(updated.elapsedMs).toBe(10_000);
			expect(updated.clock).toBe(initial.clock);
		} finally {
			nowSpy.mockRestore();
		}
	});

	it("renders a Working… row at the transcript tail when the session is running", () => {
		const now = Date.now();
		const html = renderToStaticMarkup(
			<MessageList
				entries={[turnStartedEntry("entry_turn", 1, now - 5_000)]}
				activeLeafId="entry_turn"
				isRunning
				serverTimeMs={now}
				hasSession
				sessionId="session_a"
				entriesSessionId="session_a"
			/>
		);

		expect(html).toContain("Working…");
	});

	it("omits the Working… row when the session is idle", () => {
		const html = renderToStaticMarkup(
			<MessageList
				entries={[turnStartedEntry("entry_turn", 1, Date.now() - 5_000)]}
				activeLeafId="entry_turn"
				isRunning={false}
				serverTimeMs={null}
				hasSession
				sessionId="session_a"
				entriesSessionId="session_a"
			/>
		);

		expect(html).not.toContain("Working…");
	});
});

describe("MessageList terminal turn resume actions", () => {
	it("passes the crashed turn boundary id to the resume handler", () => {
		const html = renderToStaticMarkup(
			<MessageList
				entries={[
					turnStartedEntry("start", 1, 1),
					userEntryWithParent("user", "start", "do it"),
					assistantEntry("assistant", "user", "partial"),
					turnFinishedEntry("finish", "assistant", 1, "Crashed")
				]}
				activeLeafId="finish"
				isRunning={false}
				serverTimeMs={null}
				hasSession
				sessionId="session_a"
				entriesSessionId="session_a"
				onResumeTurn={() => {}}
				resumingTurnId="finish"
			/>
		);

		expect(html).toContain("Starting");
	});
});

describe("formatElapsed", () => {
	it("formats sub-minute durations as seconds", () => {
		expect(formatElapsed(0)).toBe("0s");
		expect(formatElapsed(999)).toBe("0s");
		expect(formatElapsed(1500)).toBe("1s");
		expect(formatElapsed(59_500)).toBe("59s");
	});

	it("formats minute-scale durations with zero-padded seconds", () => {
		expect(formatElapsed(60_000)).toBe("1m 00s");
		expect(formatElapsed(65_000)).toBe("1m 05s");
		expect(formatElapsed(59 * 60_000 + 12_000)).toBe("59m 12s");
	});

	it("formats hour-scale durations with zero-padded minutes and seconds", () => {
		expect(formatElapsed(60 * 60_000)).toBe("1h 00m 00s");
		expect(formatElapsed(2 * 60 * 60_000 + 3 * 60_000 + 7_000)).toBe("2h 03m 07s");
	});

	it("clamps negative inputs to zero", () => {
		expect(formatElapsed(-1)).toBe("0s");
		expect(formatElapsed(-60_000)).toBe("0s");
	});
});

function userEntry(id: string, text: string): TranscriptEntry {
	return {
		id,
		parent_id: null,
		timestamp_ms: 0,
		item: { type: "user_message", content: [{ type: "text", text }] },
		provider_replay: []
	};
}

function memoryStorage(): TranscriptScrollStorage {
	const data = new Map<string, string>();
	return {
		getItem: (key) => data.get(key) ?? null,
		setItem: (key, value) => {
			data.set(key, value);
		},
		removeItem: (key) => {
			data.delete(key);
		}
	};
}

function userEntryWithParent(id: string, parentId: string | null, text: string): TranscriptEntry {
	return {
		id,
		parent_id: parentId,
		timestamp_ms: 0,
		item: { type: "user_message", content: [{ type: "text", text }] },
		provider_replay: []
	};
}

function assistantEntry(id: string, parentId: string | null, text: string): TranscriptEntry {
	return {
		id,
		parent_id: parentId,
		timestamp_ms: 0,
		item: { type: "assistant_message", items: [{ type: "text", text }] },
		provider_replay: []
	};
}

function assistantToolEntry(id: string, parentId: string | null, items: AssistantItem[]): TranscriptEntry {
	return {
		id,
		parent_id: parentId,
		timestamp_ms: 0,
		item: { type: "assistant_message", items },
		provider_replay: []
	};
}

function toolResultEntry(
	id: string,
	parentId: string | null,
	toolCallId: string,
	toolName: string,
	output: string,
	status: "Success" | "Error" | "Interrupted" | "Crashed" = "Success",
): TranscriptEntry {
	return {
		id,
		parent_id: parentId,
		timestamp_ms: 0,
		item: { type: "tool_result", tool_call_id: toolCallId, tool_name: toolName, output, status },
		provider_replay: []
	};
}

function turnFinishedEntry(id: string, parentId: string | null, turnId: number, outcome: "Graceful" | "Interrupted" | "Crashed"): TranscriptEntry {
	return {
		id,
		parent_id: parentId,
		timestamp_ms: 0,
		item: { type: "turn_finished", turn_id: turnId, outcome },
		provider_replay: []
	};
}

function turnStartedEntry(id: string, turnId: number, timestampMs: number): TranscriptEntry {
	return {
		id,
		parent_id: null,
		timestamp_ms: timestampMs,
		item: { type: "turn_started", turn_id: turnId },
		provider_replay: []
	};
}

function compactionSummaryEntry(
	id: string,
	parentId: string | null,
	lastTurnId: number,
	timestampMs: number,
	turnStartedAtMs?: number | null,
	sourceLeafId = "source_leaf",
): TranscriptEntry {
	return {
		id,
		parent_id: parentId,
		timestamp_ms: timestampMs,
		item: {
			type: "compaction_summary",
			source_session_id: "session_a",
			source_leaf_id: sourceLeafId,
			summary: "summary",
			tokens_before: null,
			last_turn_id: lastTurnId,
			turn_started_at_ms: turnStartedAtMs,
		},
		provider_replay: []
	};
}

