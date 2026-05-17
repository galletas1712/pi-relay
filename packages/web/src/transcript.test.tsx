import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import { assistantRenderParts, captureScrollPosition, editToolPreview, formatElapsed, isScrolledAtBottom, MessageList, restoreScrollPosition } from "./transcript.tsx";
import type { AssistantItem, ProviderReplayItem, TranscriptEntry } from "./types.ts";

describe("assistantRenderParts", () => {
	it("uses local replay display metadata even when no hosted tools are present", () => {
		const parts = assistantRenderParts(
			[toolCall("call_1", "str_replace_based_edit_tool")],
			[
				replay(
					"claude",
					{ type: "tool_use", id: "call_1", name: "str_replace_based_edit_tool" },
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

	it("renders apply_patch as an edit diff preview", () => {
		const preview = editToolPreview("apply_patch", {
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

	it("renders Claude str_replace as an edit diff preview", () => {
		const preview = editToolPreview("str_replace_based_edit_tool", {
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
});

describe("MessageList session loading guard", () => {
	it("shows a loading state instead of stale entries when entries belong to another session", () => {
		const html = renderToStaticMarkup(
			<MessageList
				entries={[userEntry("entry_1", "stale transcript text")]}
				activeLeafId="entry_1"
				isRunning={false}
				hasSession
				sessionId="session_b"
				entriesSessionId="session_a"
			/>
		);

		expect(html).toContain("Loading session");
		expect(html).not.toContain("stale transcript text");
	});
});

describe("MessageList Working indicator", () => {
	it("renders a Working… row at the transcript tail when the session is running", () => {
		const html = renderToStaticMarkup(
			<MessageList
				entries={[turnStartedEntry("entry_turn", 1, Date.now() - 5_000)]}
				activeLeafId="entry_turn"
				isRunning
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
				hasSession
				sessionId="session_a"
				entriesSessionId="session_a"
			/>
		);

		expect(html).not.toContain("Working…");
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

function turnStartedEntry(id: string, turnId: number, timestampMs: number): TranscriptEntry {
	return {
		id,
		parent_id: null,
		timestamp_ms: timestampMs,
		item: { type: "turn_started", turn_id: turnId },
		provider_replay: []
	};
}
