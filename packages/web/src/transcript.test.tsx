import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it, vi } from "vitest";
import {
	adjacentTurnJumpTargetId,
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
import type { AssistantItem, PendingAction, TranscriptEntry, TurnCard } from "./types.ts";

describe("assistantRenderParts", () => {
	it("keeps assistant text and tool-call parts in transcript order", () => {
		const parts = assistantRenderParts([
			{ type: "text", text: "hello" },
			toolCall("call_1", "Edit"),
		]);

		expect(parts).toMatchObject([
			{
				type: "text",
				item: { type: "text", text: "hello" },
			},
			{
				type: "tool_call",
				item: { type: "tool_call", id: "call_1", tool_name: "Edit" },
			},
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
				onExpandTurn={() => {}}
				onCollapseTurn={() => {}}
			/>
		);

		expect(html).toContain("before compaction");
		expect(html).toContain("Context compacted through turn 1");
		expect(html).toContain("Hide prior");
		expect(html).not.toContain("prior entries hidden");
	});
});

describe("MessageList daemon observations", () => {
	it("renders typed daemon tool observations as system messages", () => {
		const html = renderToStaticMarkup(
			<MessageList
				entries={[
					turnStartedEntry("start", 1, 1),
					{
						id: "daemon",
						parent_id: "start",
						timestamp_ms: 2,
						item: {
							type: "daemon_tool_observation",
							tool_call_id: "call_inspect_delegation_delegation_1_attempt_1",
							tool_name: "inspect_delegation",
							args_json: "{\"delegation_id\":\"delegation_1\"}",
							result_json: { delegation_id: "delegation_1", status: "done", suggested_next: "approved" },
							status: "Success",
							summary: "Delegation delegation_1 completed",
						},
					},
				]}
				activeLeafId="daemon"
				isRunning={false}
				serverTimeMs={null}
				hasSession
				sessionId="session_a"
				entriesSessionId="session_a"
			/>
		);

		expect(html).toContain("system-message info");
		expect(html).toContain("Delegation delegation_1 completed");
		expect(html).toContain("status done");
		expect(html).not.toContain("user-message");
	});

	it("renders daemon observations in the default collapsed turn-card path", () => {
		const daemonEntry: TranscriptEntry = {
			id: "daemon",
			parent_id: "start",
			timestamp_ms: 2,
			item: {
				type: "daemon_tool_observation",
				tool_call_id: "call_inspect_delegation_delegation_1_attempt_1",
				tool_name: "inspect_delegation",
				args_json: "{\"delegation_id\":\"delegation_1\"}",
				result_json: { delegation_id: "delegation_1", status: "done", suggested_next: "approved" },
				status: "Success",
				summary: "Delegation delegation_1 completed",
			},
		};
		const html = renderToStaticMarkup(
			<MessageList
				entries={[]}
				turnCards={[
					{
						card: {
							id: "turn_1",
							turn_id: 1,
							status: "open",
							outcome: null,
							start_entry_id: "start",
							boundary_entry_id: null,
							active_leaf_id: "daemon",
							start_sequence: 1,
							end_sequence: 2,
							start_timestamp_ms: 1,
							timestamp_ms: 2,
							user_messages: [],
							daemon_observations: [daemonEntry],
							assistant_message: null,
							summary: null,
							can_resume: false,
						},
						entries: null,
						expanded: false,
						isCurrent: true,
					},
				]}
				activeLeafId="daemon"
				isRunning={false}
				serverTimeMs={null}
				hasSession
				sessionId="session_a"
				entriesSessionId="session_a"
				onExpandTurn={() => {}}
				onCollapseTurn={() => {}}
			/>
		);

		expect(html).toContain("system-message info");
		expect(html).toContain("Delegation delegation_1 completed");
		expect(html).toContain("status done");
		expect(html).not.toContain("user-message");
		expect(html).not.toContain("single-tool");
	});
});

function toolCall(id: string, toolName: string): AssistantItem {
	return { type: "tool_call", id, tool_name: toolName, args_json: "{}" };
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

describe("turn jump navigation", () => {
	const targets = [
		{ id: "turn_1", top: 0, bottom: 80 },
		{ id: "turn_2", top: 320, bottom: 400 },
		{ id: "turn_3", top: 980, bottom: 1060 },
	];

	it("jumps to the nearest previous turn beginning before the current scroll position", () => {
		expect(adjacentTurnJumpTargetId(targets, 700, "previous")).toBe("turn_2");
	});

	it("jumps to the current turn user message when it is clipped above the viewport", () => {
		expect(adjacentTurnJumpTargetId(targets, 350, "previous", 400)).toBe("turn_2");
	});

	it("jumps to the previous turn user message when the current user message is fully visible", () => {
		expect(adjacentTurnJumpTargetId(targets, 320, "previous", 400)).toBe("turn_1");
	});

	it("jumps past the current turn when already at its beginning", () => {
		expect(adjacentTurnJumpTargetId(targets, 320, "previous")).toBe("turn_1");
	});

	it("jumps to the next turn beginning after the current scroll position", () => {
		expect(adjacentTurnJumpTargetId(targets, 321, "next")).toBe("turn_3");
	});

	it("renders pinned controls and DOM anchors when there are multiple turns", () => {
		const html = renderToStaticMarkup(
			<MessageList
				entries={[]}
				turnCards={[
					{ card: turnCard("turn_1", 1, "first"), entries: null, expanded: false, isCurrent: false },
					{ card: turnCard("turn_2", 2, "second"), entries: [userEntryWithParent("user_2", "start_2", "second")], expanded: true, isCurrent: false },
				]}
				activeLeafId="finish_2"
				isRunning={false}
				serverTimeMs={null}
				hasSession
				sessionId="session_a"
				entriesSessionId="session_a"
				onExpandTurn={() => {}}
				onCollapseTurn={() => {}}
			/>
		);

		expect(html).toContain("turn-jump-controls");
		expect(html).toContain("aria-label=\"Jump to previous turn\"");
		expect(html).toContain("aria-label=\"Jump to next turn\"");
		expect(html).toContain("data-turn-jump-target-id=\"turn_1\"");
		expect(html).toContain("data-turn-jump-target-id=\"turn_2\"");
		expect(html).toContain("turn-summary completed expanded");
		expect(html).toContain("Hide details");
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

	it("shows loaded details for the current running turn by default", () => {
		const bashTool = { type: "tool_call" as const, id: "call_1", tool_name: "Bash", args_json: "{\"command\":\"date\"}" };
		const start = turnStartedEntry("start", 1, 1);
		const user = userEntryWithParent("user", "start", "inspect");
		const assistant = assistantToolEntry("assistant", "user", [bashTool]);
		const html = renderToStaticMarkup(
			<MessageList
				entries={[]}
				turnCards={[
					{
						card: {
							id: "turn_1",
							turn_id: 1,
							status: "open",
							outcome: null,
							start_entry_id: "start",
							boundary_entry_id: null,
							active_leaf_id: "assistant",
							start_sequence: 1,
							end_sequence: 3,
							start_timestamp_ms: 1,
							timestamp_ms: 3,
							user_messages: [user],
							assistant_message: assistant,
							summary: null,
							can_resume: false,
						},
						entries: [start, user, assistant],
						expanded: true,
						isCurrent: true,
					},
				]}
				activeLeafId="assistant"
				isRunning
				serverTimeMs={3}
				hasSession
				sessionId="session_a"
				entriesSessionId="session_a"
				onExpandTurn={() => {}}
				onCollapseTurn={() => {}}
			/>
		);

		expect(html).toContain("Bash: date");
		expect(html).toContain("expanded");
	});

	it("keeps completed turn card details collapsed by default", () => {
		const bashTool = { type: "tool_call" as const, id: "call_1", tool_name: "Bash", args_json: "{\"command\":\"date\"}" };
		const start = turnStartedEntry("start", 1, 1);
		const user = userEntryWithParent("user", "start", "inspect");
		const assistant = assistantToolEntry("assistant", "user", [bashTool]);
		const finish = turnFinishedEntry("finish", "assistant", 1, "Graceful", 6_001);
		const html = renderToStaticMarkup(
			<MessageList
				entries={[]}
				turnCards={[
					{
						card: {
							id: "turn_1",
							turn_id: 1,
							status: "completed",
							outcome: "Graceful",
							start_entry_id: "start",
							boundary_entry_id: "finish",
							active_leaf_id: "finish",
							start_sequence: 1,
							end_sequence: 4,
							start_timestamp_ms: 1,
							timestamp_ms: 6_001,
							user_messages: [user],
							assistant_message: assistant,
							summary: null,
							can_resume: false,
						},
						entries: [start, user, assistant, finish],
						expanded: false,
						isCurrent: false,
					},
				]}
				activeLeafId="finish"
				isRunning={false}
				serverTimeMs={null}
				hasSession
				sessionId="session_a"
				entriesSessionId="session_a"
				onExpandTurn={() => {}}
				onCollapseTurn={() => {}}
			/>
		);

		expect(html).not.toContain("Bash: date");
		expect(html).toContain("Show details");
		expect(html).toContain("Worked for 6s");
	});

	it("keeps the latest tool-only assistant message visible in expanded turn details", () => {
		const bashTool = { type: "tool_call" as const, id: "call_1", tool_name: "Bash", args_json: "{\"command\":\"echo hi\"}" };
		const start = turnStartedEntry("start", 1, 1);
		const user = userEntryWithParent("user", "start", "inspect");
		const assistant = assistantToolEntry("assistant", "user", [bashTool]);
		const result = toolResultEntry("result", "assistant", "call_1", "Bash", "ok");
		const finish = turnFinishedEntry("finish", "result", 1, "Graceful");
		const html = renderToStaticMarkup(
			<MessageList
				entries={[]}
				turnCards={[
					{
						card: {
							id: "turn_1",
							turn_id: 1,
							status: "completed",
							outcome: "Graceful",
							start_entry_id: "start",
							boundary_entry_id: "finish",
							active_leaf_id: "finish",
							start_sequence: 1,
							end_sequence: 5,
							start_timestamp_ms: 1,
							timestamp_ms: 6_001,
							user_messages: [user],
							assistant_message: assistant,
							summary: null,
							can_resume: false,
						},
						entries: [start, user, assistant, result, finish],
						expanded: true,
						isCurrent: false,
					},
				]}
				activeLeafId="finish"
				isRunning={false}
				serverTimeMs={null}
				hasSession
				sessionId="session_a"
				entriesSessionId="session_a"
				onExpandTurn={() => {}}
				onCollapseTurn={() => {}}
			/>
		);

		expect(html).toContain("Bash: echo hi");
		expect(html).toContain("single-tool");
		expect(html).toContain("Worked for 6s");
	});

	it("interleaves steer messages in expanded turn details", () => {
		const start = turnStartedEntry("start", 1, 1);
		const user = userEntryWithParent("user", "start", "start work");
		const assistantProgress = assistantEntry("assistant_progress", "user", "I will inspect first.");
		const steer = userEntryWithParent("steer", "assistant_progress", "actually check tests too");
		const assistantFinal = assistantEntry("assistant_final", "steer", "Done.");
		const finish = turnFinishedEntry("finish", "assistant_final", 1, "Graceful");
		const html = renderToStaticMarkup(
			<MessageList
				entries={[]}
				turnCards={[
					{
						card: {
							id: "turn_1",
							turn_id: 1,
							status: "completed",
							outcome: "Graceful",
							start_entry_id: "start",
							boundary_entry_id: "finish",
							active_leaf_id: "finish",
							start_sequence: 1,
							end_sequence: 6,
							start_timestamp_ms: 1,
							timestamp_ms: 6_001,
							user_messages: [user, steer],
							assistant_message: assistantFinal,
							summary: null,
							can_resume: false,
						},
						entries: [start, user, assistantProgress, steer, assistantFinal, finish],
						expanded: true,
						isCurrent: false,
					},
				]}
				activeLeafId="finish"
				isRunning={false}
				serverTimeMs={null}
				hasSession
				sessionId="session_a"
				entriesSessionId="session_a"
				onExpandTurn={() => {}}
				onCollapseTurn={() => {}}
			/>
		);

		const firstUserIndex = html.indexOf("start work");
		const progressIndex = html.indexOf("I will inspect first.");
		const steerIndex = html.indexOf("actually check tests too");
		const finalIndex = html.indexOf("Done.");
		expect([firstUserIndex, progressIndex, steerIndex, finalIndex].every((index) => index !== -1)).toBe(true);
		expect(firstUserIndex).toBeLessThan(progressIndex);
		expect(progressIndex).toBeLessThan(steerIndex);
		expect(steerIndex).toBeLessThan(finalIndex);
	});

	it("renders pending tools in expanded current turn details", () => {
		const pendingActions: PendingAction[] = [
			{
				action_row_id: "action_1",
				kind: "tool",
				status: "running",
				payload: {
					id: "call_pending",
					tool_name: "Bash",
					args_json: "{\"command\":\"npm test\"}",
				},
			},
		];
		const start = turnStartedEntry("start", 1, 1);
		const user = userEntryWithParent("user", "start", "test it");
		const html = renderToStaticMarkup(
			<MessageList
				entries={[]}
				pendingActions={pendingActions}
				turnCards={[
					{
						card: {
							id: "turn_1",
							turn_id: 1,
							status: "open",
							outcome: null,
							start_entry_id: "start",
							boundary_entry_id: null,
							active_leaf_id: "user",
							start_sequence: 1,
							end_sequence: 2,
							start_timestamp_ms: 1,
							timestamp_ms: 1,
							user_messages: [user],
							assistant_message: null,
							summary: null,
							can_resume: false,
						},
						entries: [start, user],
						expanded: true,
						isCurrent: true,
					},
				]}
				activeLeafId="user"
				isRunning
				serverTimeMs={1}
				hasSession
				sessionId="session_a"
				entriesSessionId="session_a"
				onExpandTurn={() => {}}
				onCollapseTurn={() => {}}
			/>
		);

		expect(html).toContain("Bash: npm test");
		expect(html).toContain("running");
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

	it("refreshes the working clock anchor when a newer server timestamp arrives", () => {
		const nowSpy = vi.spyOn(performance, "now");
		try {
			nowSpy.mockReturnValue(1_000);
			const cached = stableWorkingElapsedMs(null, 1_000, 10_000);
			nowSpy.mockReturnValue(2_000);
			const refreshed = stableWorkingElapsedMs(cached.clock, 1_000, 20_000);

			expect(refreshed.elapsedMs).toBe(19_000);
			expect(refreshed.clock).toEqual({
				startMs: 1_000,
				serverAnchorMs: 20_000,
				clientAnchorMs: 2_000,
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

		expect(html).toContain("Working (");
	});

	it("uses the current turn card start timestamp without loading turn detail", () => {
		const now = Date.now();
		const html = renderToStaticMarkup(
			<MessageList
				entries={[]}
				turnCards={[
					{
						card: {
							id: "turn_1",
							turn_id: 1,
							status: "open",
							outcome: null,
							start_entry_id: "start",
							boundary_entry_id: null,
							active_leaf_id: "start",
							start_sequence: 1,
							end_sequence: 1,
							start_timestamp_ms: now - 5_000,
							timestamp_ms: now - 5_000,
							user_messages: [userEntryWithParent("user", "start", "do it")],
							assistant_message: null,
							summary: null,
							can_resume: false,
						},
						entries: null,
						expanded: false,
						isCurrent: true,
					},
				]}
				activeLeafId="start"
				isRunning
				serverTimeMs={now}
				hasSession
				sessionId="session_a"
				entriesSessionId="session_a"
			/>
		);

		expect(html).toContain("Working (");
		expect(html).toContain("do it");
	});

	it("offers to refetch turn details when a card is expanded but detail is missing", () => {
		const html = renderToStaticMarkup(
			<MessageList
				entries={[]}
				turnCards={[
					{
						card: {
							id: "turn_1",
							turn_id: 1,
							status: "open",
							outcome: null,
							start_entry_id: "start",
							boundary_entry_id: null,
							active_leaf_id: "start",
							start_sequence: 1,
							end_sequence: 1,
							start_timestamp_ms: Date.now(),
							timestamp_ms: Date.now(),
							user_messages: [userEntryWithParent("user", "start", "do it")],
							assistant_message: null,
							summary: null,
							can_resume: false,
						},
						entries: null,
						expanded: true,
						isCurrent: false,
					},
				]}
				activeLeafId="start"
				isRunning={false}
				serverTimeMs={null}
				hasSession
				sessionId="session_a"
				entriesSessionId="session_a"
				onExpandTurn={() => {}}
			/>
		);

		expect(html).toContain("Show details");
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

		expect(html).not.toContain("Working (");
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
	};
}

function turnCard(id: string, turnId: number, userText: string): TurnCard {
	return {
		id,
		turn_id: turnId,
		status: "completed",
		outcome: "Graceful",
		start_entry_id: `start_${turnId}`,
		boundary_entry_id: `finish_${turnId}`,
		active_leaf_id: `finish_${turnId}`,
		start_sequence: turnId,
		end_sequence: turnId,
		start_timestamp_ms: 0,
		timestamp_ms: 0,
		user_messages: [userEntryWithParent(`user_${turnId}`, `start_${turnId}`, userText)],
		assistant_message: assistantEntry(`assistant_${turnId}`, `user_${turnId}`, `answer ${turnId}`),
		summary: null,
		can_resume: false,
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
	};
}

function assistantEntry(id: string, parentId: string | null, text: string): TranscriptEntry {
	return {
		id,
		parent_id: parentId,
		timestamp_ms: 0,
		item: { type: "assistant_message", items: [{ type: "text", text }] },
	};
}

function assistantToolEntry(id: string, parentId: string | null, items: AssistantItem[]): TranscriptEntry {
	return {
		id,
		parent_id: parentId,
		timestamp_ms: 0,
		item: { type: "assistant_message", items },
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
	};
}

function turnFinishedEntry(
	id: string,
	parentId: string | null,
	turnId: number,
	outcome: "Graceful" | "Interrupted" | "Crashed",
	timestampMs = 0
): TranscriptEntry {
	return {
		id,
		parent_id: parentId,
		timestamp_ms: timestampMs,
		item: { type: "turn_finished", turn_id: turnId, outcome },
	};
}

function turnStartedEntry(id: string, turnId: number, timestampMs: number): TranscriptEntry {
	return {
		id,
		parent_id: null,
		timestamp_ms: timestampMs,
		item: { type: "turn_started", turn_id: turnId },
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
	};
}

