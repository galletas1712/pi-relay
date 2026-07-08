import { renderToStaticMarkup } from "react-dom/server";
import type { ComponentProps } from "react";
import { describe, expect, it, vi } from "vitest";
import {
	Inspector,
	LogHeader,
	projectMenuItems,
	RunBoardDelegationList,
	sessionMenuItems,
	Sidebar,
	SessionRow,
	type RunBoardCallbacks,
} from "./panels.tsx";
import type { SessionSnapshot, SessionSummary, Delegation, Project, ToolListing } from "./types.ts";

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

function renderLogHeader(overrides: Partial<Parameters<typeof LogHeader>[0]> = {}): string {
	return renderToStaticMarkup(
		<LogHeader
			archived={false}
			status="delegating"
			title="UI polish"
			parentSessionId={null}
			modelOptions={[{ id: "gpt-test", label: "GPT test" }]}
			modelValue="gpt-test"
			modelDisabled={false}
			reasoningEfforts={["minimal", "medium"]}
			reasoningEffort="medium"
			onModelChange={() => {}}
			onReasoningEffortChange={() => {}}
			rightOpen={false}
			onToggleRight={() => {}}
			{...overrides}
		/>,
	);
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
	};
}

function renderInspector(delegations: Delegation[]): string {
	return renderToStaticMarkup(
		<Inspector
			snapshot={snapshot()}
			delegations={delegations}
			hasMoreDelegations={false}
			delegationsLoading={false}
			delegationsError={null}
			runBoard={callbacks()}
			tools={[] satisfies ToolListing[]}
		/>,
	);
}

function renderRunBoardList({
	delegations,
	hasMoreDelegations = false,
	showAllDelegations = false,
}: {
	delegations: Delegation[];
	hasMoreDelegations?: boolean;
	showAllDelegations?: boolean;
}): string {
	return renderToStaticMarkup(
		<RunBoardDelegationList
			parentSessionId="parent-1"
			delegations={delegations}
			hasMoreDelegations={hasMoreDelegations}
			showAllDelegations={showAllDelegations}
			onToggleShowAllDelegations={() => {}}
			onCancelDelegation={() => {}}
		/>,
	);
}

describe("Inspector run board delegation list", () => {
	it("shows bounded newest-first delegations by default and all loaded delegations in expanded mode", () => {
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

		// Input is the backend's newest-first delegation page. The collapsed
		// Agents outline keeps the first three loaded rows and hides older rows.
		const collapsed = renderRunBoardList({ delegations });
		expect(collapsed).toContain("task 1");
		expect(collapsed).toContain("task 2");
		expect(collapsed).toContain("task 3");
		expect(collapsed).not.toContain("task 4");
		expect(collapsed).not.toContain("task 5");
		expect(collapsed).toContain("See more (2)");
		expect(collapsed).not.toContain("Show fewer");

		const expanded = renderRunBoardList({ delegations, showAllDelegations: true });
		expect(expanded).toContain("task 1");
		expect(expanded).toContain("task 2");
		expect(expanded).toContain("task 3");
		expect(expanded).toContain("task 4");
		expect(expanded).toContain("task 5");
		expect(expanded).toContain("Show fewer");
		expect(expanded).not.toContain("See more");
	});

	it("shows the expansion control when the server reports older delegations beyond the current page", () => {
		const html = renderRunBoardList({
			delegations: [1, 2, 3].map((index) => delegation({ delegation_id: `delegation-${index}`, label: `task ${index}` })),
			hasMoreDelegations: true,
		});

		expect(html).toContain("task 1");
		expect(html).toContain("task 2");
		expect(html).toContain("task 3");
		expect(html).toContain("See more");
	});

	it("does not show an expansion control when there are three or fewer delegations", () => {
		const html = renderRunBoardList({
			delegations: [1, 2, 3].map((index) => delegation({ delegation_id: `delegation-${index}`, label: `task ${index}` })),
		});

		expect(html).toContain("task 1");
		expect(html).toContain("task 2");
		expect(html).toContain("task 3");
		expect(html).not.toContain("See more");
		expect(html).not.toContain("Show fewer");
	});
});

describe("LogHeader", () => {
	it("uses an accessible status icon instead of a visible text status pill", () => {
		const html = renderLogHeader();
		expect(html).toContain("session-status-icon delegating");
		expect(html).toContain(`aria-label="delegating session"`);
		expect(html).toContain(`title="delegating session"`);
		expect(html).not.toContain("session-state delegating");
		expect(html).not.toContain(">delegating</span>");
	});

	it("omits the literal no-session title text when no session is selected", () => {
		const html = renderLogHeader({ title: null, status: null });
		expect(html).not.toContain("No session selected");
		expect(html).not.toContain("session-status-icon");
	});

	it("shows a parent-session control only when a parent id exists", () => {
		const withoutParent = renderLogHeader({ parentSessionId: null });
		expect(withoutParent).not.toContain("parent-session-link");

		const withParent = renderLogHeader({ parentSessionId: "parent-session-12345" });
		expect(withParent).toContain("log-title-group");
		expect(withParent).toContain("parent-session-link");
		expect(withParent).toContain("parent");
		expect(withParent).toContain("open parent parent-session-12345");
	});

	it("keeps model and effort labels accessible but not visible text labels", () => {
		const html = renderLogHeader();
		expect(html).toContain(`aria-label="Model"`);
		expect(html).toContain(`aria-label="Reasoning effort"`);
		expect(html).toContain(`class="sr-only">Model</span>`);
		expect(html).toContain(`class="sr-only">Reasoning effort</span>`);
		expect(html).not.toContain(">model</span>");
		expect(html).not.toContain(">effort</span>");
	});

	it("keeps the model disabled with concise accessible locked state and no verbose explanation", () => {
		const html = renderLogHeader({ modelDisabled: true, modelLocked: true });
		expect(html).toContain(`aria-label="Model, locked"`);
		expect(html).toContain(`title="Model, locked"`);
		expect(html).toContain(`disabled=""`);
		expect(html).not.toContain("Model is locked after the first transcript entry");
	});
});

describe("Inspector tabs", () => {
	it("defaults to the run-board tab and keeps debugging sections out of that panel", () => {
		const html = renderInspector([delegation({ label: "fan-out" })]);
		expect(html).toContain(`role="tablist"`);
		expect(html).toContain(`aria-label="inspector tabs"`);
		expect(html).toContain(`aria-selected="true"`);
		expect(html).toContain("Agents");
		expect(html).toContain("fan-out");
		expect(html).not.toContain("Session panel");
		expect(html).not.toContain("<h2>Session</h2>");
		expect(html).not.toContain("<h2>Pending</h2>");
		expect(html).not.toContain("<h2>Tools</h2>");
		expect(html).not.toContain("<h2>Slash</h2>");
	});

	it("does not render no-session literal text in the right-panel empty state", () => {
		const html = renderToStaticMarkup(
			<Inspector
				snapshot={null}
				delegations={[]}
				delegationsLoading={false}
				delegationsError={null}
				runBoard={callbacks()}
				tools={[] satisfies ToolListing[]}
			/>,
		);
		expect(html).not.toContain("No session selected");
	});

	it("preserves the selected-session no-work empty state", () => {
		const html = renderInspector([]);
		expect(html).toContain("No delegated work yet.");
		expect(html).not.toContain("Couldn’t load agents");
	});
});

describe("Sidebar session list loading states", () => {
	function renderSidebar(overrides: Partial<ComponentProps<typeof Sidebar>> = {}): string {
		return renderToStaticMarkup(
			<Sidebar
				connection="open"
				projects={[]}
				selectedProjectId={null}
				query=""
				showArchived={false}
				filteredSessions={[]}
				selectedId={null}
				onQueryChange={() => {}}
				onToggleArchived={() => {}}
				onNew={() => {}}
				onSelectProject={() => {}}
				onNewProject={() => {}}
				onEditProject={() => {}}
				onSelectSession={() => {}}
				onRename={() => {}}
				onArchiveToggle={() => {}}
				onDelete={() => {}}
				{...overrides}
			/>,
		);
	}

	it("shows loading instead of no sessions while the selected project list is loading", () => {
		const html = renderSidebar({ sessionsLoading: true });

		expect(html).toContain("Loading sessions…");
		expect(html).not.toContain("No sessions");
		expect(html).toContain(`aria-busy="true"`);
	});

	it("shows refreshing while an empty selected project list is being refetched", () => {
		const html = renderSidebar({ sessionsFetching: true });

		expect(html).toContain("Refreshing sessions…");
		expect(html).not.toContain("No sessions");
		expect(html).toContain(`aria-busy="true"`);

	});

	it("shows a no-data error with Retry and not the valid empty-list copy", () => {
		const html = renderSidebar({
			sessionsError: "request failed",
			onRetrySessions: () => {},
		});

		expect(html).toContain(`role="alert"`);
		expect(html).toContain("Couldn’t load sessions");
		expect(html).toContain("request failed");
		expect(html).toContain(">Retry</button>");
		expect(html).not.toContain("No sessions");
	});

	it("keeps cached rows visible when their refresh fails", () => {
		const cachedSession: SessionSummary = {
			session_id: "cached-session",
			project_id: null,
			outer_cwd: "/workspace",
			workspaces: [],
			activity: "idle",
			active_leaf_id: null,
			provider: { kind: "openai", model: "gpt-test" },
			metadata: { title: "Cached session" },
			created_at: "2024-01-01T00:00:00Z",
			updated_at: "2024-01-01T00:00:00Z",
		};
		const html = renderSidebar({
			filteredSessions: [cachedSession],
			sessionsHasCachedData: true,
			sessionsError: "refresh failed",
			onRetrySessions: () => {},
		});

		expect(html).toContain("Cached session");
		expect(html).toContain("Session refresh failed");
		expect(html).toContain("refresh failed");
	});

	it("uses unfiltered cache presence for refresh wording when filters hide every row", () => {
		const html = renderSidebar({
			filteredSessions: [],
			sessionsHasCachedData: true,
			sessionsError: "refresh failed",
			onRetrySessions: () => {},
		});

		expect(html).toContain("Session refresh failed");
		expect(html).not.toContain("Couldn’t load sessions");
		expect(html).not.toContain("No sessions");
	});

	it("disables the list Retry and reports refreshing copy while fetching", () => {
		const html = renderSidebar({
			sessionsError: "request failed",
			sessionsFetching: true,
			onRetrySessions: () => {},
		});

		expect(html).toContain(`disabled=""`);
		expect(html).toContain(`aria-busy="true"`);
		expect(html).toContain("Retrying…");
		expect(html).not.toContain(">Retry</button>");
	});

	it("renders project and session navigation as semantic lists with sibling selection and menu buttons", () => {
		const project: Project = {
			project_id: "project-1",
			name: "Menu project",
			workspaces: [{
				workspace_dir: "repo",
				kind: "git",
				remote_url: "https://example.test/repo.git",
				remote_branch: "main",
			}],
			metadata: {},
			created_at: "2024-01-01T00:00:00Z",
			updated_at: "2024-01-01T00:00:00Z",
		};
		const session: SessionSummary = {
			session_id: "session-1",
			project_id: project.project_id,
			outer_cwd: "/workspace",
			workspaces: [],
			activity: "idle",
			active_leaf_id: "leaf-123",
			provider: { kind: "openai", model: "gpt-test" },
			metadata: { title: "Menu session" },
			created_at: "2024-01-01T00:00:00Z",
			updated_at: "2024-01-01T00:00:00Z",
		};

		const html = renderSidebar({
			projects: [project],
			selectedProjectId: project.project_id,
			filteredSessions: [session],
			selectedId: session.session_id,
		});

		expect(html).toContain('<nav aria-label="Projects">');
		expect(html).toContain('<ul class="project-list">');
		expect(html).toContain('<li class="project-row selected">');
		expect(html).toContain('<nav class="session-list" aria-label="Sessions"');
		expect(html).toContain('<ul class="session-list-items">');
		expect(html).toContain('<li class="session-row selected ');
		expect(html).toContain('class="project-row-primary"');
		expect(html).toContain('class="session-row-primary"');
		expect(html.match(/aria-current="page"/g)).toHaveLength(2);
		expect(html).toContain('aria-label="Open project actions for Menu project"');
		expect(html).toContain('aria-label="Open session actions for Menu session"');
		expect(html.match(/aria-haspopup="menu"/g)).toHaveLength(2);
		expect(html).toMatch(
			/<li class="project-row selected"><button class="project-row-primary"[\s\S]*?<\/button><button class="action-menu-trigger"/,
		);
		expect(html).toMatch(
			/<li class="session-row selected [^"]*"><button class="session-row-primary"[\s\S]*?<\/button><button class="action-menu-trigger"/,
		);
		expect(html).not.toContain('role="listbox"');
		expect(html).not.toContain('role="button"');
		expect(html).toContain('aria-label="idle session"');
		expect(html).toContain("gpt-test");
		expect(html).not.toContain("leaf-1");
		expect(html).not.toContain("activity-counts");
		expect(html).not.toContain("activity-chip");
		expect(html).not.toContain("1 workspace</");
		expect(html).toContain('class="project-folder-count" role="img" aria-label="1 workspace"');
		expect(html).toContain(">1</span>");
	});

	it("names plural workspace counts on the compact folder icon without a subtitle", () => {
		const html = renderSidebar({
			projects: [{
				project_id: "project-2",
				name: "Two repos",
				workspaces: [
					{ workspace_dir: "one", kind: "local", source_path: "/one" },
					{ workspace_dir: "two", kind: "local", source_path: "/two" },
				],
				metadata: {},
				created_at: "2024-01-01T00:00:00Z",
				updated_at: "2024-01-01T00:00:00Z",
			}],
		});

		expect(html).toContain('role="img" aria-label="2 workspaces"');
		expect(html).not.toContain("2 workspaces</");
	});
});

describe("sidebar action menu policies", () => {
	it("maps project settings to the captured project without selecting the row", () => {
		const project: Project = {
			project_id: "project-1",
			name: "Menu project",
			workspaces: [],
			metadata: {},
			created_at: "2024-01-01T00:00:00Z",
			updated_at: "2024-01-01T00:00:00Z",
		};
		const onEditProject = vi.fn();
		const items = projectMenuItems(project, onEditProject);

		expect(items.map(({ id, label }) => ({ id, label }))).toEqual([
			{ id: "settings", label: "Project settings…" },
		]);
		expect(items[0]).toMatchObject({ focusDestination: "dialog" });
		items[0].onSelect();
		expect(onEditProject).toHaveBeenCalledTimes(1);
		expect(onEditProject).toHaveBeenCalledWith(project);
	});

	it("maps idle session actions, separating and styling destructive Delete", () => {
		const onRename = vi.fn();
		const onArchiveToggle = vi.fn();
		const onDelete = vi.fn();
		const items = sessionMenuItems({
			archived: false,
			canArchive: true,
			canDelete: true,
			onRename,
			onArchiveToggle,
			onDelete,
		});

		expect(items.map(({ id, label }) => ({ id, label }))).toEqual([
			{ id: "rename", label: "Rename…" },
			{ id: "archive", label: "Archive" },
			{ id: "delete", label: "Delete…" },
		]);
		expect(items[0]).toMatchObject({ focusDestination: "dialog" });
		expect(items[0].disabled).toBeUndefined();
		expect(items[1]).toMatchObject({ disabled: false });
		expect(items[1].destructive).toBeUndefined();
		expect(items[2]).toMatchObject({
			disabled: false,
			destructive: true,
			separatorBefore: true,
			focusDestination: "dialog",
		});

		items[0].onSelect();
		expect(onRename).toHaveBeenCalledTimes(1);
		expect(onArchiveToggle).not.toHaveBeenCalled();
		expect(onDelete).not.toHaveBeenCalled();

		items[1].onSelect();
		expect(onArchiveToggle).toHaveBeenCalledTimes(1);
		expect(onDelete).not.toHaveBeenCalled();

		items[2].onSelect();
		expect(onDelete).toHaveBeenCalledTimes(1);
	});

	it("uses Unarchive for archived sessions without adding a confirmation policy", () => {
		const onArchiveToggle = vi.fn();
		const items = sessionMenuItems({
			archived: true,
			canArchive: true,
			canDelete: true,
			onRename: vi.fn(),
			onArchiveToggle,
			onDelete: vi.fn(),
		});

		expect(items[1]).toMatchObject({
			id: "unarchive",
			label: "Unarchive",
			disabled: false,
		});
		expect(items[1].focusDestination).toBeUndefined();
		items[1].onSelect();
		expect(onArchiveToggle).toHaveBeenCalledTimes(1);
	});

	it("keeps Rename available and exposes visible reasons for running-session restrictions", () => {
		const items = sessionMenuItems({
			archived: false,
			canArchive: false,
			canDelete: false,
			onRename: vi.fn(),
			onArchiveToggle: vi.fn(),
			onDelete: vi.fn(),
		});

		expect(items[0].disabled).toBeUndefined();
		expect(items[1]).toMatchObject({
			label: "Archive",
			disabled: true,
			disabledReason: "Available when the session and its subagents are idle.",
		});
		expect(items[2]).toMatchObject({
			label: "Delete…",
			disabled: true,
			disabledReason: "Available when the session and its subagents are idle.",
			destructive: true,
			separatorBefore: true,
		});
	});
});

describe("Inspector agent outline", () => {
	it("renders every status as a shape-distinct accessible icon without visible status prose", () => {
		const cases: { status: Delegation["status"]; icon: string; label: string }[] = [
			{ status: "running", icon: "running", label: "running" },
			{ status: "done", icon: "done", label: "done" },
			{ status: "done_with_failures", icon: "warn", label: "done with failures" },
			{ status: "failed", icon: "failed", label: "failed" },
			{ status: "cancelled", icon: "cancelled", label: "cancelled" },
		];
		for (const { status, icon, label } of cases) {
			const html = renderRunBoardList({ delegations: [delegation({ kind: "full", status })] });
			expect(html, `${status} icon color`).toContain(`run-board-status-icon ${icon}`);
			expect(html, `${status} accessible text`).toContain(`aria-label="${label} status"`);
			expect(html).not.toContain("run-board-status-text");
			expect(html).not.toContain("Writing task");
		}
		const sample = renderRunBoardList({ delegations: [delegation({ kind: "full", status: "running" })] });
		expect(sample).not.toContain("status-rail");
	});

	it("does not expose kind labels or storage vocabulary", () => {
		const full = renderRunBoardList({ delegations: [delegation({ kind: "full", status: "done" })] });
		expect(full).not.toContain("Writing task");
		expect(full).not.toContain(">full<");

		const fanout = renderRunBoardList({ delegations: [delegation({ kind: "readonly_fanout", status: "done" })] });
		expect(fanout).not.toContain("Parallel research");
		expect(fanout).not.toContain(">fan-out<");
	});

	it("renders each agent role visibly and keeps status in the full-row navigation name", () => {
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
		expect(html).toContain(`aria-label="Open agent Agent, explorer, done"`);
		expect(html).toContain(`aria-label="Open agent Agent, explorer, running"`);
		expect(html).toContain(`aria-label="Open agent Agent, explorer, queued"`);
		expect(html).toContain(`class="run-board-subagent-button"`);
		expect(html).toContain(`title="queued"`);
		expect(html).not.toContain("activity idle");
		expect(html).not.toContain("run-board-subagent-status");
		expect(html).not.toContain("status-rail");
	});
});

describe("Inspector agent task content", () => {
	it("omits progress, outcomes, kinds, headings, and handoff metadata", () => {
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

		expect(html).toContain("fan-out");
		expect(html).not.toContain("Parallel research");
		expect(html).toContain("run-board-status-icon running");
		expect(html).not.toContain("status-rail");
		expect(html).not.toContain("run-board-progress");
		expect(html).not.toContain("2 expected");
		expect(html).not.toContain("Outcome: Done");
		expect(html).not.toContain("Needs attention");
		expect(html).not.toContain(">Active<");
		expect(html).not.toContain(">Recent<");
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
		expect(html).toContain(`aria-label="cancelled status"`);
		expect(html).not.toContain("run-board-status-text");
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
		expect(html).toContain(`aria-label="Cancel"`);
		expect(html).toContain(`aria-label="Open agent Agent, implementer, running"`);
		expect(html).not.toContain("steer");
		expect(html).not.toContain("suggested next");
	});

	it("offers only active-work cancel and never a terminal delegated-work restart", () => {
		const terminal = renderInspector([delegation({ status: "done" })]);
		expect(terminal).not.toContain(`aria-label="Cancel"`);
		expect(terminal).not.toContain("steer");

		const running = renderInspector([delegation({ status: "running" })]);
		expect(running).toContain(`aria-label="Cancel"`);
	});
});

describe("SessionRow sidebar delegating state", () => {
	function summary(overrides: Partial<SessionSummary> = {}): SessionSummary {
		return {
			session_id: "parent-1",
			project_id: null,
			outer_cwd: "/workspace",
			workspaces: [],
			activity: "idle",
			active_leaf_id: null,
			provider: { kind: "openai", model: "gpt-test" },
			metadata: {},
			created_at: "2024-01-01T00:00:00Z",
			updated_at: "2024-01-01T00:00:00Z",
			...overrides,
		};
	}

	function renderRow(session: SessionSummary): string {
		return renderToStaticMarkup(
			<SessionRow
				session={session}
				selected={false}
				onSelect={() => {}}
				onRename={() => {}}
				onArchiveToggle={() => {}}
				onDelete={() => {}}
			/>,
		);
	}

	it("renders the delegating rail and disables archive/delete for an idle parent with running subagents", () => {
		const html = renderRow(summary({ activity: "idle", has_running_delegations: true }));
		// Third state: idle parent parked behind a running delegation.
		expect(html).toContain("status-rail delegating");
		expect(html).not.toContain("status-rail idle");
		expect(html).toContain('aria-label="delegating session"');
		// Closed Radix portals do not SSR their item content. The session menu
		// policy tests above cover disabled archive/delete and their visible reason.
		expect(html).toContain('aria-label="Open session actions for Untitled session"');
	});

	it("keeps the idle rail and enables archive/delete when no delegations run", () => {
		const html = renderRow(summary({ activity: "idle", has_running_delegations: false }));
		expect(html).toContain("status-rail idle");
		expect(html).not.toContain("status-rail delegating");
		expect(html).toContain('aria-label="idle session"');
		expect(html).toContain('aria-label="Open session actions for Untitled session"');
	});

	it("shows the running rail when the parent itself is active", () => {
		const html = renderRow(summary({ activity: "running", has_running_delegations: false }));
		expect(html).toContain("status-rail running");
		expect(html).not.toContain("status-rail delegating");
	});
});
