import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import { Inspector, RunBoardDelegationList, type RunBoardCallbacks } from "./panels.tsx";
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

function renderRunBoardList({
	delegations,
	showAllDelegations = false,
	openDebugDelegationIds = new Set<string>(),
}: {
	delegations: Delegation[];
	showAllDelegations?: boolean;
	openDebugDelegationIds?: ReadonlySet<string>;
}): string {
	return renderToStaticMarkup(
		<RunBoardDelegationList
			delegations={delegations}
			showAllDelegations={showAllDelegations}
			openDebugDelegationIds={openDebugDelegationIds}
			openFile={null}
			onToggleShowAllDelegations={() => {}}
			onToggleDelegationDebug={() => {}}
			onOpenFile={() => {}}
			onCloseFile={() => {}}
			onCancelDelegation={() => {}}
			onReRunDelegation={() => {}}
		/>,
	);
}

describe("Inspector run board delegation list", () => {
	it("shows only the first three delegations by default and all delegations in expanded mode", () => {
		const delegations = Array.from({ length: 5 }, (_, index) =>
			delegation({
				delegation_id: `delegation-${index + 1}`,
				label: `task ${index + 1}`,
				handoff_dir: `/workspace/.pi-handoff/delegation-${index + 1}`,
			}),
		);

		const collapsed = renderRunBoardList({ delegations });
		expect(collapsed).toContain("task 1");
		expect(collapsed).toContain("task 2");
		expect(collapsed).toContain("task 3");
		expect(collapsed).not.toContain("task 4");
		expect(collapsed).not.toContain("task 5");
		expect(collapsed).toContain("see more (2)");
		expect(collapsed).not.toContain("show fewer");

		const expanded = renderRunBoardList({ delegations, showAllDelegations: true });
		expect(expanded).toContain("task 1");
		expect(expanded).toContain("task 2");
		expect(expanded).toContain("task 3");
		expect(expanded).toContain("task 4");
		expect(expanded).toContain("task 5");
		expect(expanded).toContain("show fewer");
		expect(expanded).not.toContain("see more");
	});

	it("does not show an expansion control when there are three or fewer delegations", () => {
		const html = renderRunBoardList({
			delegations: [1, 2, 3].map((index) => delegation({ delegation_id: `delegation-${index}`, label: `task ${index}` })),
		});

		expect(html).toContain("task 1");
		expect(html).toContain("task 2");
		expect(html).toContain("task 3");
		expect(html).not.toContain("see more");
		expect(html).not.toContain("show fewer");
	});
});

describe("Inspector run board handoff details", () => {
	it("hides handoff paths and artifact file names in the default completed-delegation render", () => {
		const html = renderInspector([
			delegation({
				status: "done_with_failures",
				subagents: [
					{
						id: "child-1",
						status: "done",
						role: "reviewer",
						subagent_type: "read_only",
						task: "review the change",
						final_message: "Reviewed the patch.",
						suggested_next: "ship it",
					},
				],
			}),
		]);

		expect(html).not.toContain("handoff /workspace/.pi-handoff/delegation-1");
		expect(html).not.toContain("/workspace/.pi-handoff/delegation-1");
		expect(html).not.toContain("final_message.md");
		expect(html).not.toContain("transcript.md");
		expect(html).not.toContain("index.json");
		expect(html).toContain("Reviewed the patch.");
		expect(html).toContain("suggested next");
		expect(html).toContain("ship it");
	});

	it("reveals artifact actions and handoff path only in debug details", () => {
		const html = renderRunBoardList({
			delegations: [delegation({ status: "done_with_failures" })],
			openDebugDelegationIds: new Set(["delegation-1"]),
		});

		expect(html).toContain("handoff /workspace/.pi-handoff/delegation-1");
		expect(html).toContain("final_message.md");
		expect(html).toContain("transcript.md");
		expect(html).not.toContain("index.json");
	});

	it("keeps cancellation transcript debug-only", () => {
		const cancelled = delegation({
			status: "cancelled",
			subagents: [
				{
					id: "child-1",
					status: "idle",
					role: "reviewer",
					subagent_type: "read_only",
					task: "review the change",
					cancellation_transcript_relative_path: "cancelled/child-1.transcript.md",
				},
			],
		});

		const initial = renderRunBoardList({ delegations: [cancelled] });
		expect(initial).not.toContain("handoff /workspace/.pi-handoff/delegation-1");
		expect(initial).not.toContain("cancellation transcript");
		expect(initial).not.toContain("cancelled/child-1.transcript.md");
		expect(initial).not.toContain("index.json");

		const debug = renderRunBoardList({
			delegations: [cancelled],
			openDebugDelegationIds: new Set(["delegation-1"]),
		});
		expect(debug).toContain("handoff /workspace/.pi-handoff/delegation-1");
		expect(debug).toContain("cancellation transcript");
		expect(debug).toContain("cancelled/child-1.transcript.md");
		expect(debug).not.toContain("index.json");
	});

	it("renders a terminal subagent summary in a still-running fan-out", () => {
		const html = renderRunBoardList({
			delegations: [
				delegation({
					status: "running",
					kind: "readonly_fanout",
					label: "fan-out",
					subagents: [
						{
							id: "done-child",
							status: "done",
							activity: "idle",
							role: "explorer",
							subagent_type: "read_only",
							task: "explore one angle",
							final_message: "Found the answer.\n\nsuggested_next: done",
							suggested_next: "done",
						},
						{
							id: "running-child",
							status: "running",
							activity: "running",
							role: "explorer",
							subagent_type: "read_only",
							task: "explore another angle",
							final_message: "This should stay hidden while running.",
							suggested_next: "done",
						},
					],
				}),
			],
		});

		expect(html).toContain("fan-out");
		expect(html).toContain("Found the answer.");
		expect(html).toContain("suggested next");
		expect(html).toContain("done");
		expect(html).not.toContain("This should stay hidden while running.");
		expect(html).not.toContain("final_message.md");
		expect(html).not.toContain("transcript.md");
	});
});

describe("Inspector run board primary controls", () => {
	it("does not offer steer from the run board while keeping cancel and subagent open controls", () => {
		const html = renderInspector([
			delegation({
				kind: "full",
				status: "running",
				label: "implement",
				subagents: [
					{
						id: "child-full-1",
						status: "running",
						activity: "running",
						role: "implementer",
						subagent_type: "full",
						steerable: true,
						task: "implement the change",
					},
				],
			}),
		]);

		expect(html).toContain("implement");
		expect(html).toContain("cancel");
		expect(html).toContain("open child-full-1");
		expect(html).not.toContain("steer");
	});

	it("keeps the re-run control for terminal re-runnable delegations", () => {
		const html = renderInspector([delegation({ status: "done" })]);

		expect(html).toContain("re-run");
		expect(html).not.toContain("steer");
	});
});
