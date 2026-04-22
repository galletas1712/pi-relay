import { describe, expect, it } from "vitest";
import type { SessionEntry, SessionMessageEntry } from "../../src/core/session-manager.js";
import {
	buildSessionContext,
	buildSessionTree,
	getLatestSessionName,
	getSessionBranch,
} from "../../src/core/session-tree.js";

function msg(
	id: string,
	parentId: string | null,
	role: "user" | "assistant",
	text: string,
	timestamp = "2025-01-01T00:00:00Z",
): SessionMessageEntry {
	const base = { type: "message" as const, id, parentId, timestamp };
	if (role === "user") {
		return { ...base, message: { role, content: text, timestamp: 1 } };
	}
	return {
		...base,
		message: {
			role,
			content: [{ type: "text", text }],
			api: "anthropic-messages",
			provider: "anthropic",
			model: "claude-test",
			usage: {
				input: 1,
				output: 1,
				cacheRead: 0,
				cacheWrite: 0,
				totalTokens: 2,
				cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 },
			},
			stopReason: "stop",
			timestamp: 1,
		},
	};
}

describe("session-tree helpers", () => {
	it("buildSessionContext returns empty context for an explicit null leaf", () => {
		const entries: SessionEntry[] = [msg("1", null, "user", "hello")];

		expect(buildSessionContext(entries, null)).toEqual({
			messages: [],
			thinkingLevel: "off",
			model: null,
		});
	});

	it("getSessionBranch reconstructs a root-to-leaf path from the entry map", () => {
		const entries: SessionEntry[] = [
			msg("1", null, "user", "start"),
			msg("2", "1", "assistant", "reply"),
			msg("3", "2", "user", "follow up"),
		];
		const byId = new Map(entries.map((entry) => [entry.id, entry]));

		expect(getSessionBranch(byId, "3").map((entry) => entry.id)).toEqual(["1", "2", "3"]);
	});

	it("buildSessionTree promotes orphaned entries to roots and sorts siblings by timestamp", () => {
		const entries: SessionEntry[] = [
			msg("root", null, "user", "root", "2025-01-01T00:00:00Z"),
			msg("newer", "root", "assistant", "newer", "2025-01-01T00:00:03Z"),
			msg("older", "root", "assistant", "older", "2025-01-01T00:00:01Z"),
			msg("orphan", "missing", "user", "orphan", "2025-01-01T00:00:02Z"),
		];
		const labelsById = new Map([
			["older", "checkpoint"],
			["orphan", "lost"],
		]);
		const labelTimestampsById = new Map([
			["older", "2025-01-01T00:10:00Z"],
			["orphan", "2025-01-01T00:11:00Z"],
		]);

		const tree = buildSessionTree(entries, labelsById, labelTimestampsById);
		const rootNode = tree.find((node) => node.entry.id === "root");
		const orphanNode = tree.find((node) => node.entry.id === "orphan");

		expect(tree).toHaveLength(2);
		expect(rootNode?.children.map((child) => child.entry.id)).toEqual(["older", "newer"]);
		expect(rootNode?.children[0]?.label).toBe("checkpoint");
		expect(rootNode?.children[0]?.labelTimestamp).toBe("2025-01-01T00:10:00Z");
		expect(orphanNode?.label).toBe("lost");
		expect(orphanNode?.labelTimestamp).toBe("2025-01-01T00:11:00Z");
	});

	it("getLatestSessionName trims names and respects explicit clears", () => {
		const entries: Array<{ type: string; name?: string }> = [
			{ type: "session_info", name: "  First  " },
			{ type: "message" },
			{ type: "session_info", name: "   " },
		];

		expect(getLatestSessionName(entries)).toBeUndefined();
		expect(
			getLatestSessionName([
				...entries,
				{ type: "session_info", name: "  Final name  " },
			]),
		).toBe("Final name");
	});
});
