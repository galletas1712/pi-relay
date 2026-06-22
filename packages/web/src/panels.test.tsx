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
		progress: { expected: 1, spawned: 1, terminal: 1, running: 0, failed: 0 },
		handoff_dir: "/workspace/.pi-handoff/delegation-1",
		subagents: [
			{
				id: "child-1",
				status: "done",
				activity: "idle",
				role: "reviewer",
				subagent_type: "read_only",
				task_prompt_file: "child-1/task_prompt.md",
				final_message_file: "child-1/final_message.md",
				transcript_file: "child-1/transcript.md",
				suggested_next: "approved",
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
				subagents: [
					{
						id: `child-${index + 1}`,
						status: "done",
						activity: "idle",
						role: "reviewer",
						subagent_type: "read_only",
						task_prompt_file: `child-${index + 1}/task_prompt.md`,
						final_message_file: `child-${index + 1}/final_message.md`,
						transcript_file: `child-${index + 1}/transcript.md`,
						suggested_next: "approved",
					},
				],
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
						activity: "idle",
						role: "reviewer",
						subagent_type: "read_only",
						task_prompt_file: "child-1/task_prompt.md",
						final_message_file: "child-1/final_message.md",
						transcript_file: "child-1/transcript.md",
						suggested_next: "approved",
					},
				],
			}),
		]);

		expect(html).not.toContain("handoff /workspace/.pi-handoff/delegation-1");
		expect(html).not.toContain("/workspace/.pi-handoff/delegation-1");
		expect(html).not.toContain("final_message.md");
		expect(html).not.toContain("transcript.md");
		expect(html).not.toContain("task_prompt.md");
		expect(html).not.toContain("index.json");
		expect(html).toContain("suggested next");
		expect(html).toContain("approved");
		expect(html).not.toContain("Reviewed the patch");
	});

	it("reveals artifact actions and handoff path only in debug details", () => {
		const html = renderRunBoardList({
			delegations: [delegation({ status: "done_with_failures" })],
			openDebugDelegationIds: new Set(["delegation-1"]),
		});

		expect(html).toContain("handoff /workspace/.pi-handoff/delegation-1");
		expect(html).toContain("task_prompt.md");
		expect(html).toContain("final_message.md");
		expect(html).toContain("transcript.md");
		expect(html).toContain("task prompt");
		expect(html).toContain("final message");
		expect(html).toContain("transcript");
		expect(html).not.toContain("index.json");
	});

	it("keeps cancellation transcript debug-only while preserving status", () => {
		const cancelled = delegation({
			status: "cancelled",
			subagents: [
				{
					id: "child-1",
					status: "cancelled",
					activity: "idle",
					role: "reviewer",
					subagent_type: "read_only",
					task_prompt_file: "child-1/task_prompt.md",
					transcript_file: "cancelled/child-1.transcript.md",
				},
			],
		});

		const initial = renderRunBoardList({ delegations: [cancelled] });
		expect(initial).toContain("cancelled");
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

	it("renders compact terminal subagent status and suggested_next without inline prose", () => {
		const html = renderRunBoardList({
			delegations: [
				delegation({
					status: "running",
					kind: "readonly_fanout",
					label: "fan-out",
					progress: { expected: 2, spawned: 2, terminal: 1, running: 1, failed: 0 },
					subagents: [
						{
							id: "done-child",
							status: "done",
							activity: "idle",
							role: "explorer",
							subagent_type: "read_only",
							task_prompt_file: "done-child/task_prompt.md",
							transcript_file: null,
							final_message_file: null,
							suggested_next: "done",
						},
						{
							id: "running-child",
							status: "running",
							activity: "running",
							role: "explorer",
							subagent_type: "read_only",
							task_prompt_file: "running-child/task_prompt.md",
							transcript_file: null,
							suggested_next: null,
						},
					],
				}),
			],
		});

		expect(html).toContain("fan-out");
		expect(html).toContain("1/2 terminal, 1 running, 0 failed");
		expect(html).toContain("done</span>");
		expect(html).toContain("idle</span>");
		expect(html).toContain("suggested next");
		expect(html).toContain("done");
		expect(html).not.toContain("Found the answer");
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
						task_prompt_file: "child-full-1/task_prompt.md",
						transcript_file: "child-full-1/transcript.md",
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
