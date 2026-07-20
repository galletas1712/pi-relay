import { renderToStaticMarkup } from "react-dom/server";
import type { ComponentProps } from "react";
import { describe, expect, it, vi } from "vitest";
import {
	Inspector,
	LogHeader,
	NoticeStack,
	RunBoardDelegationList,
	sessionMenuItems,
	Sidebar,
	SessionRow,
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
		runtime_id: "runtime-test",
	workspace_id: "workspace-test",
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

function renderInspector(delegations: Delegation[]): string {
	return renderToStaticMarkup(
		<Inspector
			snapshot={snapshot()}
			delegations={delegations}
			hasMoreDelegations={false}
			delegationsLoading={false}
			delegationsError={null}
			onCancelDelegation={() => {}}
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

	it("uses server paging metadata without inventing expansion for a complete short page", () => {
		const html = renderRunBoardList({
			delegations: [1, 2, 3].map((index) => delegation({ delegation_id: `delegation-${index}`, label: `task ${index}` })),
			hasMoreDelegations: true,
		});

		expect(html).toContain("task 1");
		expect(html).toContain("task 2");
		expect(html).toContain("task 3");
		expect(html).toContain("See more");

		const complete = renderRunBoardList({
			delegations: [1, 2, 3].map((index) => delegation({ delegation_id: `delegation-${index}`, label: `task ${index}` })),
		});
		expect(complete).not.toContain("See more");
		expect(complete).not.toContain("Show fewer");
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

	it("keeps the model disabled with concise accessible locked state and no verbose explanation", () => {
		const html = renderLogHeader({ modelDisabled: true, modelLocked: true });
		expect(html).toContain(`aria-label="Model, locked"`);
		expect(html).toContain(`title="Model, locked"`);
		expect(html).toContain(`disabled=""`);
		expect(html).not.toContain("Model is locked after the first transcript entry");
	});
});

describe("Error notices", () => {
	it("renders a dismiss control for an expiring error", () => {
		const html = renderToStaticMarkup(
			<NoticeStack
				notices={[{ id: "error-1", text: "Could not save" }]}
				rightOpen={false}
				onDismiss={() => {}}
			/>,
		);

		expect(html).toContain("Could not save");
		expect(html).toContain('aria-label="Dismiss notification"');
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
			runtime_id: "runtime-test",
	workspace_id: "workspace-test",
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

		expect(html).toContain('<nav aria-label="Projects"');
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
		expect(html).not.toContain("project-folder-count");
		expect(html).not.toContain("1 workspace");
		expect(html).not.toContain("connection-pill");
		expect(html).not.toContain(">connected<");
	});

	it("renders accessible active-session counts only for active projects", () => {
		const activeProject: Project = {
			project_id: "project-active",
			name: "Active project",
			workspaces: [],
			metadata: {},
			created_at: "2024-01-01T00:00:00Z",
			updated_at: "2024-01-01T00:00:00Z",
		};
		const inactiveProject: Project = {
			...activeProject,
			project_id: "project-inactive",
			name: "Inactive project",
		};

		const html = renderSidebar({
			projects: [activeProject, inactiveProject],
			projectActiveSessionCounts: new Map([
				[activeProject.project_id, 2],
				[inactiveProject.project_id, 0],
			]),
		});

		expect(html).toContain(
			'<span class="project-active-session-count" title="2 active sessions" aria-label="2 active sessions">2</span>',
		);
		expect(html).not.toContain('title="0 active sessions"');
	});

});

describe("sidebar action menu policies", () => {
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

describe("SessionRow sidebar delegating state", () => {
	function summary(overrides: Partial<SessionSummary> = {}): SessionSummary {
		return {
			session_id: "parent-1",
			project_id: null,
			runtime_id: "runtime-test",
	workspace_id: "workspace-test",
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

	it.each([
		["idle", false, "idle"],
		["idle", true, "delegating"],
		["running", false, "running"],
	] as const)("renders %s/child-running=%s as the %s rail", (activity, childRunning, expected) => {
		const html = renderRow(summary({ activity, has_running_delegations: childRunning }));
		expect(html).toContain(`status-rail ${expected}`);
		expect(html).toContain(`aria-label="${expected} session"`);
		expect(html).toContain('aria-label="Open session actions for Untitled session"');
	});
});
