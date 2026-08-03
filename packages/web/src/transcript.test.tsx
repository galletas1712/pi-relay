import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it, vi } from "vitest";
import {
	adjacentTranscriptNavigationTarget,
	assistantRenderParts,
	deriveTranscriptDisplayNodes,
	editToolPreview,
	formatElapsed,
	isScrolledAtBottom,
	MessageList,
	runningTurnClockAnchor,
	runningTurnStartMs,
	stableWorkingElapsedMs,
	ToolOutput,
} from "./transcript.tsx";
import { ArtifactImage, ArtifactImageProvider } from "./artifactImage.tsx";
import type { AssistantItem, PendingAction, TranscriptEntry, TurnCard } from "./types.ts";
import { buildTurnViews } from "./turnView.ts";

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

	it("renders artifact images through the shared loader with accessible fallback markup", () => {
		const artifactId = `sha256:${"a".repeat(64)}`;
		const html = renderToStaticMarkup(
			<ArtifactImageProvider load={async () => ({
				artifact_id: artifactId,
				mime_type: "image/png",
				byte_length: 8,
				data: "iVBORw0KGgo=",
			})}>
				<ArtifactImage artifactId={artifactId} />
			</ArtifactImageProvider>,
		);

		expect(html).toContain("Loading image");
		expect(html).not.toContain("data:image");
		expect(html).not.toContain("https://");
	});
});

describe("transcript display model", () => {
	it("keeps tool-result association equivalent when the indexed call IDs are reused", () => {
		const call = toolCall("call_1", "Bash");
		const entries = [
			assistantToolEntry("assistant", null, [call]),
			toolResultEntry("result", "assistant", "call_1", "Bash", "ok"),
			toolResultEntry("orphan", "result", "call_missing", "Bash", "not associated"),
		];
		const turns = buildTurnViews(entries);
		const toolResults = new Map([["call_1", entries[1].item as Extract<TranscriptEntry["item"], { type: "tool_result" }>]]);
		const expected = deriveTranscriptDisplayNodes(entries, turns);
		const optimized = deriveTranscriptDisplayNodes(entries, turns, toolResults, [], new Set(["call_1"]));

		expect(optimized).toEqual(expected);
	});

	it("copies the complete assistant entry for each separated text part", () => {
		const entries = [
			assistantToolEntry("assistant", null, [
				{ type: "text", text: "before " },
				toolCall("call_1", "Bash"),
				{ type: "text", text: "after" },
			]),
		];

		expect(deriveTranscriptDisplayNodes(entries)).toEqual(
			expect.arrayContaining([
				expect.objectContaining({ type: "assistant_text", text: "before ", copyText: "before after" }),
				expect.objectContaining({ type: "assistant_text", text: "after", copyText: "before after" }),
			]),
		);
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

	it("does not render marked replay messages in the fallback transcript", () => {
		const original = userEntryWithParent("original", "start", "exact compaction instruction");
		const replayed: TranscriptEntry = {
			...userEntryWithParent("replayed", "compact", "exact compaction instruction"),
			item: {
				type: "user_message",
				content: [{ type: "text", text: "exact compaction instruction" }],
				replayed_after_compaction: true,
			},
		};
		const replayedAgain: TranscriptEntry = {
			...userEntryWithParent("replayed-again", "compact-again", "exact compaction instruction"),
			item: {
				type: "user_message",
				content: [{ type: "text", text: "exact compaction instruction" }],
				replayed_after_compaction: true,
			},
		};
		const html = renderToStaticMarkup(
			<MessageList
				entries={[
					turnStartedEntry("start", 1, 1),
					original,
					compactionSummaryEntry("compact", null, 1, 2, 1, "original"),
					replayed,
					compactionSummaryEntry("compact-again", null, 1, 3, 1, "replayed"),
					replayedAgain,
				]}
				activeLeafId="replayed-again"
				isRunning={false}
				serverTimeMs={null}
				hasSession
				sessionId="session_a"
				entriesSessionId="session_a"
			/>
		);

		expect(html.match(/exact compaction instruction/g)).toHaveLength(1);
	});

	it("defensively filters marked replay messages from collapsed and expanded turn cards", () => {
		const start = turnStartedEntry("start", 1, 1);
		const original = userEntryWithParent("original", "start", "only once");
		const replayed: TranscriptEntry = {
			...userEntryWithParent("replayed", "original", "only once"),
			item: {
				type: "user_message",
				content: [{ type: "text", text: "only once" }],
				replayed_after_compaction: true,
			},
		};
		const genuineSameText = userEntryWithParent("genuine", "replayed", "only once");
		const card: TurnCard = {
			id: "turn_1",
			turn_id: 1,
			status: "open",
			outcome: null,
			start_entry_id: "start",
			boundary_entry_id: null,
			active_leaf_id: "genuine",
			start_sequence: 1,
			end_sequence: 4,
			start_timestamp_ms: 1,
			timestamp_ms: 4,
			user_messages: [original, replayed, genuineSameText],
			assistant_message: null,
			summary: null,
			can_resume: false,
		};

		for (const expanded of [false, true]) {
			const html = renderToStaticMarkup(
				<MessageList
					entries={[]}
					turnCards={[
						{
							card,
							entries: expanded ? [start, original, replayed, genuineSameText] : null,
							expanded,
							isCurrent: true,
						},
					]}
					activeLeafId="genuine"
					isRunning={false}
					serverTimeMs={null}
					hasSession
					sessionId="session_a"
					entriesSessionId="session_a"
				/>
			);

			expect(html.match(/only once/g)).toHaveLength(2);
		}
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
							result_json: { delegation_id: "delegation_1", status: "done", outcome: "approved" },
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
				result_json: { delegation_id: "delegation_1", status: "done", outcome: "approved" },
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

describe("turn jump navigation", () => {
	const stops = [
		{ id: "turn_1-user", top: 0, bottom: 80 },
		{ id: "turn_1-assistant", top: 120, bottom: 500 },
		{ id: "turn_2-user", top: 520, bottom: 600 },
	];

	it("visits the current assistant endpoint before the next rendered stop", () => {
		expect(adjacentTranscriptNavigationTarget(stops, 0, "next", 200)).toEqual({ id: "turn_1-assistant", edge: "start" });
		expect(adjacentTranscriptNavigationTarget(stops, 120, "next", 200)).toEqual({ id: "turn_1-assistant", edge: "end" });
		expect(adjacentTranscriptNavigationTarget(stops, 400, "next", 200)).toEqual({ id: "turn_2-user", edge: "start" });
	});

	it("returns to a clipped current stop before moving to the previous stop", () => {
		expect(adjacentTranscriptNavigationTarget(stops, 300, "previous", 200)).toEqual({ id: "turn_1-assistant", edge: "start" });
		expect(adjacentTranscriptNavigationTarget(stops, 120, "previous", 200)).toEqual({ id: "turn_1-user", edge: "start" });
		expect(adjacentTranscriptNavigationTarget(stops, 0, "previous", 200)).toBeNull();
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
		expect(html).toContain("aria-label=\"Jump to top\"");
		expect(html).toContain("aria-label=\"Jump to bottom\"");
		expect(html).toContain("aria-label=\"Jump to previous turn\"");
		expect(html).toContain("aria-label=\"Jump to next turn\"");
		expect(html).toContain('data-transcript-nav-stop="user-turn_1-user_1"');
		expect(html).toContain('data-transcript-nav-stop="assistant-turn_1"');
		expect(html).toContain("turn-summary completed expanded");
		expect(html).toContain("Hide details");
	});

	it("walks rendered stops in order and visits a long assistant endpoint before later content", () => {
		const stops = [
			{ id: "system-prompt", top: 0, bottom: 40 },
			{ id: "user", top: 60, bottom: 110 },
			{ id: "assistant", top: 120, bottom: 500 },
			{ id: "next-user", top: 520, bottom: 570 },
		];

		expect(adjacentTranscriptNavigationTarget(stops, 0, "next", 100)).toEqual({ id: "user", edge: "start" });
		expect(adjacentTranscriptNavigationTarget(stops, 60, "next", 100)).toEqual({ id: "assistant", edge: "start" });
		expect(adjacentTranscriptNavigationTarget(stops, 120, "next", 100)).toEqual({ id: "assistant", edge: "end" });
		expect(adjacentTranscriptNavigationTarget(stops, 400, "next", 100)).toEqual({ id: "next-user", edge: "start" });
	});

	it("walks visible stops in reverse without exposing absent system prompts", () => {
		const stops = [
			{ id: "user", top: 60, bottom: 110 },
			{ id: "assistant", top: 120, bottom: 500 },
		];

		expect(adjacentTranscriptNavigationTarget(stops, 200, "previous", 100)).toEqual({ id: "assistant", edge: "start" });
		expect(adjacentTranscriptNavigationTarget(stops, 120, "previous", 100)).toEqual({ id: "user", edge: "start" });
		expect(adjacentTranscriptNavigationTarget(stops, 0, "previous", 100)).toBeNull();
	});

	it("marks only rendered system prompt, fallback nodes, and card blocks as navigation stops", () => {
		const withPrompt = renderToStaticMarkup(
			<MessageList
				entries={[userEntry("entry_1", "first user message")]}
				activeLeafId="entry_1"
				isRunning={false}
				serverTimeMs={null}
				hasSession
				sessionId="session_a"
				entriesSessionId="session_a"
				transcriptStartContent={<div>See system prompt</div>}
			/>,
		);
		const withoutPrompt = renderToStaticMarkup(
			<MessageList
				entries={[userEntry("entry_1", "first user message")]}
				activeLeafId="entry_1"
				isRunning={false}
				serverTimeMs={null}
				hasSession
				sessionId="session_a"
				entriesSessionId="session_a"
				hasOlderTurns
				transcriptStartContent={<div>See system prompt</div>}
			/>,
		);
		const collapsed = renderToStaticMarkup(
			<MessageList
				entries={[]}
				turnCards={[{ card: turnCard("turn_1", 1, "first"), entries: null, expanded: false, isCurrent: false }]}
				activeLeafId="finish_1"
				isRunning={false}
				serverTimeMs={null}
				hasSession
				sessionId="session_a"
				entriesSessionId="session_a"
			/>,
		);
		const expanded = renderToStaticMarkup(
			<MessageList
				entries={[]}
				turnCards={[{
					card: turnCard("turn_1", 1, "first"),
					entries: [
						userEntryWithParent("user_1", "start_1", "first"),
						assistantEntry("detail-assistant", "user_1", "intermediate"),
					],
					expanded: true,
					isCurrent: false,
				}]}
				activeLeafId="finish_1"
				isRunning={false}
				serverTimeMs={null}
				hasSession
				sessionId="session_a"
				entriesSessionId="session_a"
			/>,
		);

		expect(withPrompt.match(/data-transcript-nav-stop=/g)).toHaveLength(2);
		expect(withPrompt).toContain('data-transcript-nav-stop="system-prompt"');
		expect(withoutPrompt).not.toContain("data-transcript-nav-stop=\"system-prompt\"");
		expect(collapsed.match(/data-transcript-nav-stop=/g)).toEqual([
			"data-transcript-nav-stop=",
			"data-transcript-nav-stop=",
		]);
		expect(collapsed).toContain('data-transcript-nav-stop="user-turn_1-user_1"');
		expect(collapsed).toContain('data-transcript-nav-stop="assistant-turn_1"');
		expect(expanded).toContain('data-transcript-nav-stop="detail-turn_1-user_1"');
		expect(expanded).toContain('data-transcript-nav-stop="detail-turn_1-detail-assistant-item-0"');
		expect(expanded).toContain('data-transcript-nav-stop="assistant-turn_1"');
		expect(expanded).not.toContain('data-transcript-nav-stop="user-turn_1-user_1"');
	});
});

describe("MessageList session loading guard", () => {
	it("shows the system prompt control before the first user content only when the oldest page is loaded", () => {
		const oldestPage = renderToStaticMarkup(
			<MessageList
				entries={[userEntry("entry_1", "first user message")]}
				activeLeafId="entry_1"
				isRunning={false}
				serverTimeMs={null}
				hasSession
				sessionId="session_a"
				entriesSessionId="session_a"
				transcriptStartContent={<div>See system prompt</div>}
			/>,
		);
		const pagedTail = renderToStaticMarkup(
			<MessageList
				entries={[userEntry("entry_1", "later user message")]}
				activeLeafId="entry_1"
				isRunning={false}
				serverTimeMs={null}
				hasSession
				sessionId="session_a"
				entriesSessionId="session_a"
				hasOlderTurns
				transcriptStartContent={<div>See system prompt</div>}
			/>,
		);

		expect(oldestPage.indexOf("See system prompt")).toBeLessThan(oldestPage.indexOf("first user message"));
		expect(pagedTail).not.toContain("See system prompt");
	});

	it("shows the system prompt control for a loaded durable empty session, but not new-session state", () => {
		const durable = renderToStaticMarkup(
			<MessageList
				entries={[]}
				activeLeafId={null}
				isRunning={false}
				serverTimeMs={null}
				hasSession
				sessionId="session_a"
				entriesSessionId="session_a"
				transcriptStartContent={<div>See system prompt</div>}
			/>,
		);
		const unsaved = renderToStaticMarkup(
			<MessageList
				entries={[]}
				activeLeafId={null}
				isRunning={false}
				serverTimeMs={null}
				hasSession={false}
				transcriptStartContent={<div>See system prompt</div>}
			/>,
		);

		expect(durable).toContain("See system prompt");
		expect(unsaved).not.toContain("See system prompt");
	});

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

		expect(html).toContain("Loading conversation…");
		expect(html).not.toContain("stale transcript text");
	});

	it("replaces an initial load failure with a persistent Retry action", () => {
		const html = renderToStaticMarkup(
			<MessageList
				entries={[]}
				activeLeafId={null}
				isRunning={false}
				serverTimeMs={null}
				hasSession
				sessionId="session_b"
				entriesSessionId={null}
				sessionError="daemon unavailable"
				onRetrySession={() => {}}
			/>,
		);

		expect(html).toContain(`role="alert"`);
		expect(html).toContain("Couldn’t load session");
		expect(html).toContain("daemon unavailable");
		expect(html).toContain(">Retry</button>");
		expect(html).not.toContain("Loading conversation");
	});

	it("keeps matching cached content visible with a refresh warning and Retry", () => {
		const html = renderToStaticMarkup(
			<MessageList
				entries={[userEntry("entry_1", "cached transcript text")]}
				activeLeafId="entry_1"
				isRunning={false}
				serverTimeMs={null}
				hasSession
				sessionId="session_a"
				entriesSessionId="session_a"
				sessionError="refresh timed out"
				sessionErrorHasUsableCache
				onRetrySession={() => {}}
			/>,
		);

		expect(html).toContain("cached transcript text");
		expect(html).toContain("Session refresh failed");
		expect(html).toContain("refresh timed out");
		expect(html).toContain(">Retry</button>");
	});

	it("disables the selected-session action and reports busy copy while retrying", () => {
		const html = renderToStaticMarkup(
			<MessageList
				entries={[]}
				activeLeafId={null}
				isRunning={false}
				serverTimeMs={null}
				hasSession
				sessionId="session_b"
				entriesSessionId={null}
				sessionError="daemon unavailable"
				retryingSession
				onRetrySession={() => {}}
			/>,
		);

		expect(html).toContain(`disabled=""`);
		expect(html).toContain(`aria-busy="true"`);
		expect(html).toContain("Retrying…");
		expect(html).toContain("daemon unavailable");
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
		const html = renderToStaticMarkup(<ToolOutput result={{ type: "tool_result", tool_call_id: "call_1", tool_name: "Bash", status: "Success", content: [{ type: "text", text: output }] }} />);

		expect(html).toContain("line 60");
		expect(html).not.toContain("\\n...");
	});
});

describe("MessageList tool use cards", () => {
	it("prefers call_description titles while retaining historical command fallbacks", () => {
		const described = {
			type: "tool_call" as const,
			id: "call_1",
			tool_name: "Bash",
			args_json: "{\"call_description\":\"Inspect the checked-out files.\",\"command\":\"ls -la\"}"
		};
		const historical = {
			type: "tool_call" as const,
			id: "call_2",
			tool_name: "Bash",
			args_json: "{\"command\":\"pwd\"}"
		};
		const html = renderToStaticMarkup(
			<MessageList
				entries={[
					turnStartedEntry("start", 1, 1),
					userEntryWithParent("user", "start", "inspect"),
					assistantToolEntry("assistant", "user", [described, historical])
				]}
				activeLeafId="assistant"
				isRunning
				serverTimeMs={null}
				hasSession
				sessionId="session_a"
				entriesSessionId="session_a"
			/>
		);

		expect(html).toContain("Inspect the checked-out files.");
		expect(html).not.toContain("Bash: Inspect the checked-out files.");
		expect(html).toContain("Bash: pwd");
		expect(html).not.toContain("Bash: ls -la");
	});

	it("does not present an MCP operational call_description as a relay explanation", () => {
		const mcpCall = {
			type: "tool_call" as const,
			id: "call_mcp",
			tool_name: "mcp__fixture__operate",
			args_json: "{\"call_description\":\"server operation mode\",\"value\":7}"
		};
		const html = renderToStaticMarkup(
			<MessageList
				entries={[
					turnStartedEntry("start", 1, 1),
					userEntryWithParent("user", "start", "operate"),
					assistantToolEntry("assistant", "user", [mcpCall])
				]}
				activeLeafId="assistant"
				isRunning
				serverTimeMs={null}
				hasSession
				sessionId="session_a"
				entriesSessionId="session_a"
			/>
		);

		expect(html).toContain('<span class="tool-run-item-title">mcp__fixture__operate</span>');
		expect(html).not.toContain('<span class="tool-run-item-title">server operation mode</span>');
	});

	it("keeps non-Bash first-party titles on their existing fallbacks", () => {
		const calls = [
			{
				type: "tool_call" as const,
				id: "call_edit",
				tool_name: "Edit",
				args_json: "{\"call_description\":\"Apply the requested edit.\",\"input\":\"*** Begin Patch\\n*** Add File: tmp/example.txt\\n+hello\\n*** End Patch\\n\"}",
			},
			{
				type: "tool_call" as const,
				id: "call_web",
				tool_name: "WebSearch",
				args_json: "{\"call_description\":\"Search for the requested information.\",\"query\":\"rust\"}",
			},
			{
				type: "tool_call" as const,
				id: "call_delegation",
				tool_name: "inspect_delegation",
				args_json: "{\"call_description\":\"Inspect the delegation.\",\"delegation_id\":\"delegation_1\"}",
			},
		];
		const html = renderToStaticMarkup(
			<MessageList
				entries={[
					turnStartedEntry("start", 1, 1),
					userEntryWithParent("user", "start", "operate"),
					assistantToolEntry("assistant", "user", calls),
				]}
				activeLeafId="assistant"
				isRunning
				serverTimeMs={null}
				hasSession
				sessionId="session_a"
				entriesSessionId="session_a"
			/>
		);

		expect(html).toContain("Edited example.txt +1");
		expect(html).toContain('<span class="tool-run-item-title">WebSearch</span>');
		expect(html).toContain('<span class="tool-run-item-title">inspect_delegation</span>');
		expect(html).not.toContain("Apply the requested edit.");
		expect(html).not.toContain("Search for the requested information.");
		expect(html).not.toContain("Inspect the delegation.");
	});

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
		expect(html).not.toContain('data-transcript-nav-stop="duration-turn_1"');
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
					args_json: "{\"call_description\":\"Run the web test suite.\",\"command\":\"npm test\"}",
				},
			},
			{
				action_row_id: "action_non_bash",
				kind: "tool",
				status: "running",
				payload: {
					id: "call_pending_web",
					tool_name: "WebSearch",
					args_json: "{\"call_description\":\"Search while waiting.\",\"query\":\"rust\"}",
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

		expect(html).toContain("Run the web test suite.");
		expect(html).not.toContain("Bash: Run the web test suite.");
		expect(html).not.toContain("Bash: npm test");
		expect(html).toContain('<span class="tool-run-item-title">WebSearch</span>');
		expect(html).not.toContain("Search while waiting.");
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

	it("renders non-Graceful fallback status rows with resume actions without making them stops", () => {
		const html = renderToStaticMarkup(
			<MessageList
				entries={[
					turnStartedEntry("start", 1, 1),
					userEntryWithParent("user", "start", "do it"),
					assistantEntry("assistant", "user", "partial"),
					turnFinishedEntry("finish", "assistant", 1, "Crashed"),
				]}
				activeLeafId="finish"
				isRunning={false}
				serverTimeMs={null}
				hasSession
				sessionId="session_a"
				entriesSessionId="session_a"
				onResumeTurn={() => {}}
			/>,
		);

		expect(html).toContain("turn 1 crashed");
		expect(html).toContain("Retry");
		expect(html.match(/data-transcript-nav-stop=/g)).toHaveLength(2);
		expect(html).not.toContain('data-transcript-nav-stop="fallback-finish"');
		expect(html).not.toContain("data-turn-jump");
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
		item: {
			type: "tool_result",
			tool_call_id: toolCallId,
			tool_name: toolName,
			content: [{ type: "text", text: output }],
			status,
		},
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

