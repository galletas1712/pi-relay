import { describe, expect, it } from "vitest";
import { pendingInputIsReflected, type PendingInput } from "./pendingInputs.ts";
import type { ProviderConfig, SessionSnapshot } from "./types.ts";

const provider: ProviderConfig = { kind: "openai", model: "gpt-5.1" };

describe("pending inputs", () => {
	it("treats accepted optimistic transcript bubbles as reflected once they are stale", () => {
		const input = pendingInput({
			status: "accepted",
			submittedAt: 1_000,
		});

		expect(pendingInputIsReflected(input, snapshot([]), 5_000)).toBe(false);
		expect(pendingInputIsReflected(input, snapshot([]), 12_001)).toBe(true);
	});

	it("does not expire sending or queued optimistic inputs without canonical state", () => {
		for (const status of ["sending", "queued"] as const) {
			const input = pendingInput({ status, submittedAt: 1_000 });

			expect(pendingInputIsReflected(input, snapshot([]), 30_000)).toBe(false);
		}
	});
});

function pendingInput(options: Partial<PendingInput> = {}): PendingInput {
	return {
		id: "pending_1",
		sessionId: "session_1",
		clientInputId: "web_1",
		content: [{ type: "text", text: "hello" }],
		placement: "transcript",
		priority: "follow_up",
		status: "accepted",
		submittedAt: 1_000,
		...options,
	};
}

function snapshot(entries: SessionSnapshot["entries"] = []): SessionSnapshot {
	return {
		session_id: "session_1",
		project_id: "project_1",
		outer_cwd: "/repo",
		workspaces: [],
		activity: "running",
		active_leaf_id: entries.at(-1)?.id ?? null,
		provider,
		metadata: {},
		pending_actions: [],
		queued_inputs: [],
		session_revision: 1,
		queue_revision: 1,
		transcript_revision: 1,
		last_event_id: 1,
		server_time_ms: 2_000,
		has_transcript_entries: entries.length > 0,
		entries,
	};
}
