import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import { Inspector, type RunBoardCallbacks } from "./panels.tsx";
import type { HandoffFileName, SessionSnapshot, Delegation, ToolListing } from "./types.ts";

function delegation(overrides: Partial<Delegation> = {}): Delegation {
	return {
		delegation_id: "delegation-1",
		kind: "readonly_fanout",
		status: "done",
		workflow: null,
		label: "review",
		handoff_dir: "/workspace/.pi-handoff/delegation-1",
		subagents: [
			{
				id: "child-1",
				status: "idle",
				role: "reviewer",
				subagent_type: "read_only",
				task: "review the change",
			},
		],
		...overrides,
	};
}

function snapshot(): SessionSnapshot {
	return {
		session_id: "parent-1",
		project_id: null,
		outer_cwd: "/workspace",
		workspaces: [],
		activity: "idle",
		active_leaf_id: null,
		provider: { kind: "openai", model: "gpt-test" },
		metadata: {},
		pending_actions: [],
		queued_inputs: [],
		last_event_id: 1,
		server_time_ms: 1_700_000_000_000,
	};
}

function callbacks(): Omit<RunBoardCallbacks, "onSelectSession"> {
	return {
		onCancelDelegation: () => {},
		onSteerSubagent: () => {},
		onReRunDelegation: () => {},
		readHandoffFile: (_delegationId: string, _subagentId: string | null, _file: HandoffFileName) => Promise.resolve(""),
	};
}

function renderInspector(delegations: Delegation[]): string {
	return renderToStaticMarkup(
		<Inspector
			snapshot={snapshot()}
			delegations={delegations}
			delegationsLoading={false}
			delegationsError={null}
			runBoard={callbacks()}
			tools={[] satisfies ToolListing[]}
		/>,
	);
}

describe("Inspector run board handoff links", () => {
	it("shows handoff path and file buttons for completed delegations", () => {
		const html = renderInspector([delegation({ status: "done_with_failures" })]);

		expect(html).toContain("handoff /workspace/.pi-handoff/delegation-1");
		expect(html).not.toContain("index.json");
		expect(html).toContain("final message");
		expect(html).toContain("transcript");
	});

	it("shows cancellation transcript links for cancelled delegations when the artifact is reported", () => {
		const html = renderInspector([
			delegation({
				status: "cancelled",
				subagents: [
					{
						id: "child-1",
						status: "idle",
						role: "reviewer",
						subagent_type: "read_only",
						task: "review the change",
						transcript_file: "cancelled/child-1.transcript.md",
					},
				],
			}),
		]);

		expect(html).toContain("handoff /workspace/.pi-handoff/delegation-1");
		expect(html).not.toContain("index.json");
		expect(html).not.toContain("final message");
		expect(html).toContain("cancellation transcript");
	});

	it("does not show file buttons for cancelled delegations without a cancellation transcript artifact", () => {
		const html = renderInspector([delegation({ status: "cancelled" })]);

		expect(html).not.toContain("handoff /workspace/.pi-handoff/delegation-1");
		expect(html).not.toContain("index.json");
		expect(html).not.toContain("final message");
		expect(html).not.toContain("cancellation transcript");
		expect(html).not.toContain("transcript");
	});

	it("does not show handoff path or file buttons for failed delegations", () => {
		const html = renderInspector([delegation({ status: "failed" })]);

		expect(html).not.toContain("handoff /workspace/.pi-handoff/delegation-1");
		expect(html).not.toContain("index.json");
		expect(html).not.toContain("final message");
		expect(html).not.toContain("transcript");
	});
});
