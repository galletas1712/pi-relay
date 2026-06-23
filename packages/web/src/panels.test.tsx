import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import { Inspector, RunBoardDelegationList, type RunBoardCallbacks } from "./panels.tsx";
import type { SessionSnapshot, Delegation, ToolListing } from "./types.ts";

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
				outcome: "approved",
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
}: {
	delegations: Delegation[];
	showAllDelegations?: boolean;
}): string {
	return renderToStaticMarkup(
		<RunBoardDelegationList
			delegations={delegations}
			showAllDelegations={showAllDelegations}
			onToggleShowAllDelegations={() => {}}
			onCancelDelegation={() => {}}
			onReRunDelegation={() => {}}
		/>,
	);
}

describe("Inspector run board delegation list", () => {
	it("shows only the three most recently launched delegations by default and all delegations in expanded mode", () => {
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
						outcome: "approved",
					},
				],
			}),
		);

		// Input is oldest-first (task 1..5); the board renders newest-first, so the
		// collapsed view keeps the three most recent (task 5/4/3) and hides 1/2.
		const collapsed = renderRunBoardList({ delegations });
		expect(collapsed).toContain("task 5");
		expect(collapsed).toContain("task 4");
		expect(collapsed).toContain("task 3");
		expect(collapsed).not.toContain("task 2");
		expect(collapsed).not.toContain("task 1");
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

describe("Inspector run board status icons", () => {
	it("encodes each delegation status as a colored, accessibly-labeled kind icon", () => {
		const cases: { status: Delegation["status"]; icon: string; label: string }[] = [
			{ status: "running", icon: "running", label: "running" },
			{ status: "done", icon: "done", label: "done" },
			{ status: "done_with_failures", icon: "warn", label: "done with failures" },
			{ status: "failed", icon: "failed", label: "failed" },
			{ status: "cancelled", icon: "cancelled", label: "cancelled" },
		];
		for (const { status, icon, label } of cases) {
			// A full delegation so the accessible name carries kind + status.
			const html = renderRunBoardList({ delegations: [delegation({ kind: "full", status })] });
			const expected = `full delegation — ${label}`;
			expect(html, `${status} icon color`).toContain(`run-board-status-icon ${icon}`);
			expect(html, `${status} icon aria-label`).toContain(`aria-label="${expected}"`);
			expect(html, `${status} icon title`).toContain(`title="${expected}"`);
			expect(html, `${status} icon role`).toContain('role="img"');
		}
		// The run board no longer renders the old vertical status rail.
		const sample = renderRunBoardList({ delegations: [delegation({ kind: "full", status: "running" })] });
		expect(sample).not.toContain("status-rail");
	});

	it("encodes the delegation kind in the icon's accessible name (full vs fan-out)", () => {
		const full = renderRunBoardList({ delegations: [delegation({ kind: "full", status: "done" })] });
		expect(full).toContain(`aria-label="full delegation — done"`);

		const fanout = renderRunBoardList({ delegations: [delegation({ kind: "readonly_fanout", status: "done" })] });
		expect(fanout).toContain(`aria-label="fan-out delegation — done"`);
	});

	it("colors each subagent's agent icon by status with the human-readable status as its accessible name", () => {
		const html = renderRunBoardList({
			delegations: [
				delegation({
					status: "running",
					subagents: [
						{ id: "done-child", status: "done", activity: "idle", role: "explorer", subagent_type: "read_only" },
						{ id: "running-child", status: "running", activity: "running", role: "explorer", subagent_type: "read_only" },
						{ id: "waiting-child", status: "queued", activity: "queued", role: "explorer", subagent_type: "read_only" },
					],
				}),
			],
		});

		// done -> done, running -> running, queued -> neutral pending icon.
		expect(html).toContain(`run-board-status-icon done`);
		expect(html).toContain(`run-board-status-icon running`);
		expect(html).toContain(`run-board-status-icon pending`);
		expect(html).toContain(`aria-label="done"`);
		expect(html).toContain(`aria-label="running"`);
		expect(html).toContain(`aria-label="queued"`);
		expect(html).toContain(`title="queued"`);
		// The vertical status rail is gone from the run board.
		expect(html).not.toContain("status-rail");
	});
});

describe("Inspector run board streamlined content", () => {
	it("drops the status pills, progress counts, outcome, and handoff/artifact clutter", () => {
		const html = renderInspector([
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
						final_message_file: "done-child/final_message.md",
						transcript_file: "done-child/transcript.md",
						outcome: "done",
					},
					{
						id: "running-child",
						status: "running",
						activity: "running",
						role: "explorer",
						subagent_type: "read_only",
						task_prompt_file: "running-child/task_prompt.md",
						outcome: null,
					},
				],
			}),
		]);

		// Title survives; the delegation kind is now carried by the status icon's
		// accessible name rather than a text tag.
		expect(html).toContain("fan-out");
		expect(html).toContain(`aria-label="fan-out delegation — running"`);
		expect(html).not.toContain("run-board-delegation-kind");
		// Status is carried only by the colored icon now (no vertical rail).
		expect(html).toContain("run-board-status-icon running");
		expect(html).not.toContain("status-rail");
		// Removed: the activity/status pill, progress text, outcome, handoff path, artifact names.
		expect(html).not.toContain("subagent-activity");
		expect(html).not.toContain("run-board-progress");
		expect(html).not.toContain("run-board-subagent-summary");
		expect(html).not.toContain("1/2 terminal, 1 running, 0 failed");
		expect(html).not.toContain("suggested next");
		expect(html).not.toContain("handoff /workspace/.pi-handoff/delegation-1");
		expect(html).not.toContain("/workspace/.pi-handoff/delegation-1");
		expect(html).not.toContain("final_message.md");
		expect(html).not.toContain("transcript.md");
		expect(html).not.toContain("task_prompt.md");
	});

	it("removes the details toggle and any artifact file viewer", () => {
		const html = renderRunBoardList({ delegations: [delegation({ status: "done_with_failures" })] });

		expect(html).not.toContain("details");
		expect(html).not.toContain("hide details");
		expect(html).not.toContain("run-board-debug");
		expect(html).not.toContain("run-board-handoff-links");
		expect(html).not.toContain("run-board-handoff-path");
		expect(html).not.toContain("run-board-file");
	});

	it("keeps a cancelled delegation's status on its icon without exposing the cancellation transcript", () => {
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

		const html = renderRunBoardList({ delegations: [cancelled] });
		expect(html).toContain("run-board-status-icon cancelled");
		expect(html).toContain(`aria-label="cancelled"`);
		expect(html).not.toContain("status-rail");
		expect(html).not.toContain("cancellation transcript");
		expect(html).not.toContain("cancelled/child-1.transcript.md");
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
		expect(html).not.toContain("suggested next");
	});

	it("keeps the re-run control for terminal re-runnable delegations and not while running", () => {
		const terminal = renderInspector([delegation({ status: "done" })]);
		expect(terminal).toContain("re-run");
		expect(terminal).not.toContain("cancel");
		expect(terminal).not.toContain("steer");

		const running = renderInspector([delegation({ status: "running" })]);
		expect(running).toContain("cancel");
		expect(running).not.toContain("re-run");
	});
});
