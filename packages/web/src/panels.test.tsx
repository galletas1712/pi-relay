import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import { Inspector, type RunBoardCallbacks } from "./panels.tsx";
import type { HandoffFileName, SessionSnapshot, Stage, ToolListing } from "./types.ts";

function stage(overrides: Partial<Stage> = {}): Stage {
	return {
		stage_id: "stage-1",
		kind: "readonly_fanout",
		status: "done",
		workflow: null,
		label: "review",
		handoff_dir: "/workspace/.pi-handoff/stage-1",
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
		onCancelStage: () => {},
		onSteerSubagent: () => {},
		onReRunStage: () => {},
		readHandoffFile: (_stageId: string, _subagentId: string | null, _file: HandoffFileName) => Promise.resolve(""),
	};
}

function renderInspector(stages: Stage[]): string {
	return renderToStaticMarkup(
		<Inspector
			snapshot={snapshot()}
			stages={stages}
			stagesLoading={false}
			stagesError={null}
			runBoard={callbacks()}
			tools={[] satisfies ToolListing[]}
		/>,
	);
}

describe("Inspector run board handoff links", () => {
	it("shows handoff path and file buttons for completed stages", () => {
		const html = renderInspector([stage({ status: "done_with_failures" })]);

		expect(html).toContain("handoff /workspace/.pi-handoff/stage-1");
		expect(html).toContain("index.json");
		expect(html).toContain("final message");
		expect(html).toContain("transcript");
	});

	it("does not show handoff path or file buttons for cancelled stages", () => {
		const html = renderInspector([stage({ status: "cancelled" })]);

		expect(html).not.toContain("handoff /workspace/.pi-handoff/stage-1");
		expect(html).not.toContain("index.json");
		expect(html).not.toContain("final message");
		expect(html).not.toContain("transcript");
	});

	it("does not show handoff path or file buttons for failed stages", () => {
		const html = renderInspector([stage({ status: "failed" })]);

		expect(html).not.toContain("handoff /workspace/.pi-handoff/stage-1");
		expect(html).not.toContain("index.json");
		expect(html).not.toContain("final message");
		expect(html).not.toContain("transcript");
	});
});
