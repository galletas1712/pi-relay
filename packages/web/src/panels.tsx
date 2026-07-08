import {
	Archive,
	ArchiveRestore,
	ArrowUp,
	Bot,
	Folder,
	Network,
	PanelRightOpen,
	Plus,
	RotateCcw,
	Search,
	Square,
	SquarePen,
	Trash2,
	X
} from "lucide-react";
import { memo, useEffect, useRef, useState, type RefObject } from "react";
import { ActionMenu, type ActionMenuItem } from "./actionMenu.tsx";
import { COMMANDS } from "./slash.ts";
import {
	isArchivedSession,
	projectTitle,
	sessionStatusWithDelegations,
	sessionTitle,
	type SessionDisplayActivity,
	type SessionStatus,
	type SessionListItem
} from "./sessionList.ts";
import { truncate } from "./text.ts";
import {
	canReRunDelegation,
	isDelegationRunning,
	delegationStatusLabel,
	statusRailClass,
} from "./delegationBoard.ts";
import type {
	Notice,
	Project,
	ReasoningEffort,
	SessionSnapshot,
	Delegation,
	DelegationSubagent,
	ToolListing,
} from "./types.ts";

function projectWorkspaceSummary(project: Project): string {
	return project.workspaces
		.map((workspace) =>
			(workspace.kind ?? "git") === "local"
				? `${workspace.workspace_dir}: local ${workspace.source_path ?? ""}`
				: `${workspace.workspace_dir}: git ${workspace.remote_url ?? ""}#${workspace.remote_branch ?? ""}`,
		)
		.join("\n");
}

export function SidebarHeader({
	connection,
	onClose
}: {
	connection: string;
	onClose?: () => void;
}) {
	const connected = connection === "open";
	return (
		<div className="sidebar-header">
			<div className="connection-row">
				<span className={`connection-pill ${connected ? "online" : "offline"}`}>
					{connected ? "connected" : connection}
				</span>
				<button className="plain-close-button sidebar-close" type="button" onClick={onClose} aria-label="close sidebar">
					<X size={14} />
				</button>
			</div>
		</div>
	);
}

export interface RunBoardCallbacks {
	onSelectSession?: (sessionId: string) => void;
	onCancelDelegation: (delegationId: string) => void;
	onReRunDelegation: (delegation: Delegation) => void;
}

interface RunBoardProps extends RunBoardCallbacks {
	parentSessionId: string | null;
	delegations: Delegation[];
	hasMoreDelegations: boolean;
	loading: boolean;
	error: string | null;
	showAllDelegations: boolean;
	onToggleShowAllDelegations: () => void;
}

const RUN_BOARD_DEFAULT_DELEGATION_COUNT = 3;

function subagentStatusLabel(subagent: DelegationSubagent): string {
	const status = typeof subagent.status === "string" ? subagent.status : "idle";
	if (status === "done_with_failures") return "done with failures";
	return status.replaceAll("_", " ");
}

function SubagentRow({
	subagent,
	onSelectSession,
}: {
	subagent: DelegationSubagent;
	onSelectSession?: (sessionId: string) => void;
}) {
	const status = typeof subagent.status === "string" ? subagent.status : "idle";
	const statusLabel = subagentStatusLabel(subagent);
	return (
		<div className="run-board-subagent" role="listitem">
			<div className="run-board-subagent-head">
				<button
					className="link-button"
					type="button"
					onClick={() => onSelectSession?.(subagent.id)}
					title={`open ${subagent.id}`}
				>
					<span
						className={`run-board-status-icon ${statusRailClass(status)}`}
						role="img"
						aria-label={statusLabel}
						title={statusLabel}
					>
						<Bot size={16} aria-hidden />
					</span>{" "}
					{subagent.role ?? subagent.id.slice(0, 13)}
				</button>
			</div>
		</div>
	);
}

function DelegationCard({
	delegation,
	canReRun,
	onSelectSession,
	onCancelDelegation,
	onReRunDelegation,
}: {
	delegation: Delegation;
	canReRun: boolean;
} & Pick<RunBoardCallbacks, "onSelectSession" | "onCancelDelegation" | "onReRunDelegation">) {
	const running = isDelegationRunning(delegation);
	const title = delegation.label ?? delegation.workflow ?? delegation.delegation_id.slice(0, 13);
	const statusLabel = delegationStatusLabel(delegation.status);
	const isFull = delegation.kind === "full";
	const KindIcon = isFull ? SquarePen : Network;
	const kindStatusLabel = `${isFull ? "full" : "fan-out"} delegation — ${statusLabel}`;
	return (
		<div className="run-board-delegation">
			<div className="run-board-delegation-head">
				<span
					className={`run-board-status-icon ${statusRailClass(delegation.status)}`}
					role="img"
					aria-label={kindStatusLabel}
					title={kindStatusLabel}
				>
					<KindIcon size={16} aria-hidden />
				</span>
				<span className="run-board-delegation-title">{title}</span>
			</div>
			<div className="run-board-delegation-controls">
				{running ? (
					<button className="chip-button" type="button" onClick={() => onCancelDelegation(delegation.delegation_id)} title="cancel this delegation">
						<Square size={11} /> cancel
					</button>
				) : null}
				{canReRun ? (
					<button className="chip-button" type="button" onClick={() => onReRunDelegation(delegation)} title="re-run this delegation">
						<RotateCcw size={11} /> re-run
					</button>
				) : null}
			</div>
			<div className="run-board-subagents" role="list">
				{delegation.subagents.map((subagent) => (
					<SubagentRow
						key={subagent.id}
						subagent={subagent}
						onSelectSession={onSelectSession}
					/>
				))}
			</div>
		</div>
	);
}

export function RunBoardDelegationList({
	delegations,
	hasMoreDelegations = false,
	showAllDelegations,
	onToggleShowAllDelegations,
	onSelectSession,
	onCancelDelegation,
	onReRunDelegation,
}: {
	delegations: Delegation[];
	hasMoreDelegations?: boolean;
	showAllDelegations: boolean;
	onToggleShowAllDelegations: () => void;
} & Pick<RunBoardCallbacks, "onSelectSession" | "onCancelDelegation" | "onReRunDelegation">) {
	// The daemon returns a bounded newest-first page for the board. Keep a local
	// cap as a defensive fallback when tests or cached data include extra rows.
	const hiddenLocalCount = Math.max(0, delegations.length - RUN_BOARD_DEFAULT_DELEGATION_COUNT);
	const visibleDelegations =
		showAllDelegations || hiddenLocalCount === 0
			? delegations
			: delegations.slice(0, RUN_BOARD_DEFAULT_DELEGATION_COUNT);
	const showToggle = hasMoreDelegations || hiddenLocalCount > 0 || showAllDelegations;
	return (
		<div className="run-board">
			{visibleDelegations.map((delegation) => (
				<DelegationCard
					key={delegation.delegation_id}
					delegation={delegation}
					canReRun={canReRunDelegation(delegation)}
					onSelectSession={onSelectSession}
					onCancelDelegation={onCancelDelegation}
					onReRunDelegation={onReRunDelegation}
				/>
			))}
			{showToggle ? (
				<button className="chip-button run-board-toggle" type="button" onClick={onToggleShowAllDelegations}>
					{showAllDelegations ? "show fewer" : `see more${hiddenLocalCount > 0 ? ` (${hiddenLocalCount})` : ""}`}
				</button>
			) : null}
		</div>
	);
}

function RunBoard({
	parentSessionId,
	delegations,
	hasMoreDelegations,
	loading,
	error,
	showAllDelegations,
	onToggleShowAllDelegations,
	onSelectSession,
	onCancelDelegation,
	onReRunDelegation,
}: RunBoardProps) {
	return (
		<section className="inspect-section run-board-section">
			{parentSessionId && loading ? <p className="muted">Loading delegations…</p> : null}
			{parentSessionId && error ? <p className="error-text">{error}</p> : null}
			{parentSessionId && !loading && !error && delegations.length === 0 ? <p className="muted">No delegations yet.</p> : null}
			{parentSessionId && delegations.length > 0 ? (
				<RunBoardDelegationList
					delegations={delegations}
					hasMoreDelegations={hasMoreDelegations}
					showAllDelegations={showAllDelegations}
					onToggleShowAllDelegations={onToggleShowAllDelegations}
					onSelectSession={onSelectSession}
					onCancelDelegation={onCancelDelegation}
					onReRunDelegation={onReRunDelegation}
				/>
			) : null}
		</section>
	);
}

type InspectorTab = "run-board" | "debug";

const INSPECTOR_TABS: { id: InspectorTab; label: string }[] = [
	{ id: "run-board", label: "Run board" },
	{ id: "debug", label: "Inspector" },
];

export interface SidebarProps {
	counts: Record<SessionDisplayActivity, number>;
	total: number;
	archived: number;
	connection: string;
	projects: Project[];
	selectedProjectId: string | null;
	query: string;
	showArchived: boolean;
	filteredSessions: SessionListItem[];
	selectedId: string | null;
	sessionsLoading?: boolean;
	sessionsFetching?: boolean;
	sessionsError?: string | null;
	sessionsHasCachedData?: boolean;
	inert?: boolean;
	newSessionButtonRef?: RefObject<HTMLButtonElement | null>;
	onRetrySessions?: () => void;
	onQueryChange: (query: string) => void;
	onToggleArchived: () => void;
	onNew: () => void;
	onClose?: () => void;
	onSelectProject: (projectId: string | null) => void;
	onNewProject: () => void;
	onEditProject: (project: Project) => void;
	onSelectSession: (sessionId: string) => void;
	onRename: (session: SessionListItem) => void;
	onArchiveToggle: (session: SessionListItem) => void;
	onDelete: (session: SessionListItem) => void;
}

export const Sidebar = memo(function Sidebar({
	counts,
	total,
	archived,
	connection,
	projects,
	selectedProjectId,
	query,
	showArchived,
	filteredSessions,
	selectedId,
	sessionsLoading = false,
	sessionsFetching = false,
	sessionsError = null,
	sessionsHasCachedData = false,
	inert,
	newSessionButtonRef,
	onRetrySessions,
	onQueryChange,
	onToggleArchived,
	onNew,
	onClose,
	onSelectProject,
	onNewProject,
	onEditProject,
	onSelectSession,
	onRename,
	onArchiveToggle,
	onDelete
}: SidebarProps) {
	return (
		<aside className="sidebar" data-slot="sidebar" inert={inert}>
			<SidebarHeader connection={connection} onClose={onClose} />
			<ProjectList
				projects={projects}
				selectedProjectId={selectedProjectId}
				onSelectProject={onSelectProject}
				onNewProject={onNewProject}
				onEditProject={onEditProject}
			/>
			<div className="session-section-head">
				<span>Sessions</span>
				<ActivityCounts counts={counts} archived={archived} />
			</div>
			<SidebarToolbar
				disabled={false}
				query={query}
				onQueryChange={onQueryChange}
				showArchived={showArchived}
				onToggleArchived={onToggleArchived}
				onNew={onNew}
				newSessionButtonRef={newSessionButtonRef}
			/>
			{sessionsError ? (
				<div className="load-error-banner sidebar-load-error" role="alert">
					<div>
						<strong>{sessionsHasCachedData ? "Session refresh failed" : "Couldn’t load sessions"}</strong>
						<span>{sessionsError}</span>
					</div>
					{onRetrySessions ? (
						<button
							type="button"
							className="secondary-button load-error-retry"
							disabled={sessionsFetching}
							aria-busy={sessionsFetching}
							onClick={onRetrySessions}
						>
							{sessionsFetching ? "Retrying…" : "Retry"}
						</button>
					) : null}
				</div>
			) : null}
			<nav className="session-list" aria-label="Sessions" aria-busy={sessionsLoading || sessionsFetching}>
				<ul className="session-list-items">
					{filteredSessions.map((session) => (
						<SessionRow
							key={session.session_id}
							session={session}
							selected={session.session_id === selectedId}
							onSelect={() => onSelectSession(session.session_id)}
							onRename={() => onRename(session)}
							onArchiveToggle={() => onArchiveToggle(session)}
							onDelete={() => onDelete(session)}
						/>
					))}
					{filteredSessions.length === 0 && !sessionsError ? (
						<li className="empty-list">
							{sessionsLoading ? "Loading sessions…" : sessionsFetching ? "Refreshing sessions…" : "No sessions"}
						</li>
					) : null}
				</ul>
			</nav>
		</aside>
	);
});

function pendingActionLabel(action: SessionSnapshot["pending_actions"][number]): string {
	if (action.kind !== "compaction") return action.kind;
	return action.payload.trigger === "auto" ? "auto-compaction" : "compaction";
}

export function ProjectList({
	projects,
	selectedProjectId,
	onSelectProject,
	onNewProject,
	onEditProject
}: {
	projects: Project[];
	selectedProjectId: string | null;
	onSelectProject: (projectId: string | null) => void;
	onNewProject: () => void;
	onEditProject: (project: Project) => void;
}) {
	return (
		<div className="project-section">
			<div className="project-section-head">
				<span>Projects</span>
				<button className="icon-button tiny" type="button" onClick={onNewProject} title="new project" aria-label="new project">
					<Plus size={13} />
				</button>
			</div>
			<nav aria-label="Projects">
				<ul className="project-list">
					<li className={`project-row ${selectedProjectId === null ? "selected" : ""}`}>
						<button
							className="project-row-primary"
							type="button"
							onClick={() => onSelectProject(null)}
							title="Ephemeral host sessions start from your home directory"
							aria-current={selectedProjectId === null ? "page" : undefined}
						>
							<Folder size={14} aria-hidden />
							<span className="project-main">
								<span className="project-title">Host</span>
								<span className="project-cwd">Ephemeral sessions</span>
							</span>
						</button>
					</li>
					{projects.map((project) => {
						const title = projectTitle(project);
						const selected = project.project_id === selectedProjectId;
						return (
							<li className={`project-row ${selected ? "selected" : ""}`} key={project.project_id}>
								<button
									className="project-row-primary"
									type="button"
									onClick={() => onSelectProject(project.project_id)}
									title={projectWorkspaceSummary(project)}
									aria-current={selected ? "page" : undefined}
								>
									<Folder size={14} aria-hidden />
									<span className="project-main">
										<span className="project-title">{title}</span>
										<span className="project-cwd">{project.workspaces.length} workspaces</span>
									</span>
								</button>
								<ActionMenu
									triggerLabel={`Open project actions for ${title}`}
									items={projectMenuItems(project, onEditProject)}
								/>
							</li>
						);
					})}
				</ul>
			</nav>
		</div>
	);
}

export function projectMenuItems(project: Project, onEditProject: (project: Project) => void): ActionMenuItem[] {
	return [
		{
			id: "settings",
			label: "Project settings…",
			icon: <SquarePen size={15} aria-hidden />,
			focusDestination: "dialog",
			onSelect: () => onEditProject(project),
		},
	];
}

export function SidebarToolbar({
	disabled,
	query,
	onQueryChange,
	showArchived,
	onToggleArchived,
	onNew,
	newSessionButtonRef,
}: {
	disabled: boolean;
	query: string;
	onQueryChange: (query: string) => void;
	showArchived: boolean;
	onToggleArchived: () => void;
	onNew: () => void;
	newSessionButtonRef?: RefObject<HTMLButtonElement | null>;
}) {
	const [searchOpen, setSearchOpen] = useState(false);
	const searchInputRef = useRef<HTMLInputElement | null>(null);
	const searchVisible = searchOpen || !!query.trim();

	useEffect(() => {
		if (!searchOpen || disabled) return;
		const frame = requestAnimationFrame(() => searchInputRef.current?.focus());
		return () => cancelAnimationFrame(frame);
	}, [disabled, searchOpen]);

	useEffect(() => {
		if (disabled || searchOpen) return;
		const handleKeyDown = (event: KeyboardEvent) => {
			const target = event.target as HTMLElement | null;
			const activeElement = document.activeElement as HTMLElement | null;
			const isTypingTarget =
				target instanceof HTMLInputElement ||
				target instanceof HTMLTextAreaElement ||
				target?.isContentEditable;
			if (isTypingTarget) return;
			if (!activeElement?.closest('[data-slot="sidebar"]')) return;
			if (event.key === "/" || ((event.metaKey || event.ctrlKey) && event.key.toLowerCase() === "f")) {
				event.preventDefault();
				setSearchOpen(true);
			}
		};
		window.addEventListener("keydown", handleKeyDown);
		return () => window.removeEventListener("keydown", handleKeyDown);
	}, [disabled, searchOpen]);

	return (
		<div className="sidebar-toolbar">
			<div className="toolbar-actions">
				<button ref={newSessionButtonRef} className="primary-button" type="button" onClick={onNew} disabled={disabled}>
					<Plus size={14} />
					New session
				</button>
				<button
					className={`icon-button ${searchVisible ? "pressed" : ""}`}
					type="button"
					onClick={() => {
						if (searchVisible) {
							onQueryChange("");
							setSearchOpen(false);
						} else {
							setSearchOpen(true);
						}
					}}
					disabled={disabled}
					aria-label={searchVisible ? "Close Filter Sessions" : "Filter Sessions"}
					title={searchVisible ? "Close Filter Sessions" : "Filter Sessions"}
				>
					<Search size={14} />
				</button>
				<button
					className={`icon-button ${showArchived ? "pressed" : ""}`}
					type="button"
					onClick={onToggleArchived}
					disabled={disabled}
					aria-label={showArchived ? "hide archived sessions" : "show archived sessions"}
					title={showArchived ? "hide archived sessions" : "show archived sessions"}
				>
					<Archive size={14} />
				</button>
			</div>
			{searchVisible ? (
				<label
					className="search-box"
					onBlur={(event) => {
						if (event.currentTarget.contains(event.relatedTarget)) return;
						if (!query.trim()) setSearchOpen(false);
					}}
				>
					<input
						ref={searchInputRef}
						value={query}
						onChange={(event) => onQueryChange(event.target.value)}
						onKeyDown={(event) => {
							if (event.key !== "Escape") return;
							event.preventDefault();
							if (query.trim()) onQueryChange("");
							else setSearchOpen(false);
						}}
						placeholder="Filter Sessions…"
						disabled={disabled}
					/>
					{query ? (
						<button
							className="search-clear-button"
							type="button"
							onClick={() => {
								onQueryChange("");
								searchInputRef.current?.focus();
							}}
							aria-label="clear session filter"
							title="Clear Filter Sessions"
						>
							<X size={13} />
						</button>
					) : null}
				</label>
			) : null}
		</div>
	);
}

function ActivityCounts({ counts, archived }: { counts: Record<SessionDisplayActivity, number>; archived: number }) {
	return (
		<div className="activity-counts">
			{(["running", "idle"] as SessionDisplayActivity[]).map((activity) => (
				<span className={`activity-chip ${activity}`} key={activity}>
					{activity}
					<span className="count">{counts[activity] ?? 0}</span>
				</span>
			))}
			<span className="activity-chip archived">
				archived
				<span className="count">{archived}</span>
			</span>
		</div>
	);
}

export function SessionRow({
	session,
	selected,
	onSelect,
	onRename,
	onArchiveToggle,
	onDelete
}: {
	session: SessionListItem;
	selected: boolean;
	onSelect: () => void;
	onRename: () => void;
	onArchiveToggle: () => void;
	onDelete: () => void;
}) {
	const archived = isArchivedSession(session);
	const status = sessionStatusWithDelegations(session.activity, session.has_running_delegations ?? false);
	const idleAndQuiet = session.activity === "idle" && !(session.has_running_delegations ?? false);
	const canArchive = idleAndQuiet;
	const canDelete = idleAndQuiet;
	const title = sessionTitle(session);
	const statusLabel = `${archived ? "archived" : status} session`;
	return (
		<li className={`session-row ${selected ? "selected" : ""} ${archived ? "archived" : ""}`}>
			<button
				className="session-row-primary"
				type="button"
				onClick={onSelect}
				aria-current={selected ? "page" : undefined}
			>
				<span
					className={`status-rail ${archived ? "archived" : status}`}
					role="img"
					aria-label={statusLabel}
					title={statusLabel}
				/>
				<span className="session-main">
					<span className="session-title">{title}</span>
					<span className="session-sub">
						{archived ? "archived - " : ""}{session.provider.model}
					</span>
					<span className="session-leaf">
						{session.active_leaf_id ? session.active_leaf_id.slice(0, 6) : "root"}
					</span>
				</span>
			</button>
			<ActionMenu
				triggerLabel={`Open session actions for ${title}`}
				items={sessionMenuItems({ archived, canArchive, canDelete, onRename, onArchiveToggle, onDelete })}
			/>
		</li>
	);
}

const IDLE_SESSION_ACTION_REASON = "Available when the session and its subagents are idle.";

export function sessionMenuItems({
	archived,
	canArchive,
	canDelete,
	onRename,
	onArchiveToggle,
	onDelete,
}: {
	archived: boolean;
	canArchive: boolean;
	canDelete: boolean;
	onRename: () => void;
	onArchiveToggle: () => void;
	onDelete: () => void;
}): ActionMenuItem[] {
	const ArchiveIcon = archived ? ArchiveRestore : Archive;
	return [
		{
			id: "rename",
			label: "Rename…",
			icon: <SquarePen size={15} aria-hidden />,
			focusDestination: "dialog",
			onSelect: onRename,
		},
		{
			id: archived ? "unarchive" : "archive",
			label: archived ? "Unarchive" : "Archive",
			icon: <ArchiveIcon size={15} aria-hidden />,
			disabled: !canArchive,
			disabledReason: !canArchive ? IDLE_SESSION_ACTION_REASON : undefined,
			onSelect: onArchiveToggle,
		},
		{
			id: "delete",
			label: "Delete…",
			icon: <Trash2 size={15} aria-hidden />,
			disabled: !canDelete,
			disabledReason: !canDelete ? IDLE_SESSION_ACTION_REASON : undefined,
			destructive: true,
			separatorBefore: true,
			focusDestination: "dialog",
			onSelect: onDelete,
		},
	];
}

export function LogHeader({
	archived,
	status,
	title,
	parentSessionId,
	modelOptions,
	modelValue,
	modelDisabled,
	modelDisabledTitle,
	reasoningEfforts,
	reasoningEffort,
	onModelChange,
	onReasoningEffortChange,
	onSelectSession,
	rightOpen,
	onToggleRight
}: {
	archived: boolean;
	status: SessionStatus | null;
	title: string | null;
	parentSessionId?: string | null;
	modelOptions: { id: string; label: string; description?: string }[];
	modelValue: string;
	modelDisabled: boolean;
	modelDisabledTitle: string;
	reasoningEfforts: ReasoningEffort[];
	reasoningEffort: ReasoningEffort;
	onModelChange: (value: string) => void;
	onReasoningEffortChange: (value: ReasoningEffort) => void;
	onSelectSession?: (sessionId: string) => void;
	rightOpen: boolean;
	onToggleRight: () => void;
}) {
	const statusLabel = archived ? "archived session" : status ? `${status} session` : null;
	return (
		<div className="log-header">
			{title ? (
				<span
					className={`session-status-icon ${archived ? "archived" : status ?? "idle"}`}
					role="img"
					aria-label={statusLabel ?? undefined}
					title={statusLabel ?? undefined}
				>
					<Bot size={14} aria-hidden />
				</span>
			) : null}
			{title ? (
				<span className="log-title-group">
					<span className="log-session">
						{title}
					</span>
					{parentSessionId ? (
						<button
							className="parent-session-link"
							type="button"
							onClick={() => onSelectSession?.(parentSessionId)}
							title={`open parent ${parentSessionId}`}
						>
							<ArrowUp size={12} aria-hidden />
							parent
						</button>
					) : null}
				</span>
			) : null}
			<div className="log-controls">
				<label className="header-select" title={modelDisabledTitle}>
					<span className="sr-only">Model</span>
					<select
						value={modelValue}
						disabled={modelDisabled}
						title={modelDisabledTitle}
						aria-label="Model"
						onChange={(event) => onModelChange(event.target.value)}
					>
						{modelOptions.map((option) => (
							<option key={option.id} value={option.id} title={option.description}>{option.label}</option>
						))}
					</select>
				</label>
				<label className="header-select compact">
					<span className="sr-only">Reasoning effort</span>
					<select
						value={reasoningEffort}
						title="Reasoning effort"
						aria-label="Reasoning effort"
						onChange={(event) => onReasoningEffortChange(event.target.value as ReasoningEffort)}
					>
						{reasoningEfforts.map((effort) => (
							<option key={effort} value={effort}>{effort}</option>
						))}
					</select>
				</label>
			</div>
			{rightOpen ? null : (
				<button
					className="icon-button tiny"
					type="button"
					onClick={onToggleRight}
					title="open inspector"
					aria-label="open inspector"
				>
					<PanelRightOpen size={14} />
				</button>
			)}
		</div>
	);
}

export function NoticeStack({ notices, rightOpen }: { notices: Notice[]; rightOpen: boolean }) {
	if (notices.length === 0) return null;
	return (
		<div className={`notice-stack ${rightOpen ? "with-inspector" : ""}`} aria-live="polite">
			{notices.slice(-4).map((notice) => (
				<div className={`notice-toast ${notice.tone}`} key={notice.id}>
					{notice.text}
				</div>
			))}
		</div>
	);
}

export function Inspector({
	snapshot,
	delegations,
	hasMoreDelegations = false,
	delegationsLoading,
	delegationsError,
	showAllDelegations = false,
	onToggleShowAllDelegations = () => {},
	runBoard,
	tools,
	onSelectSession,
	onClose
}: {
	snapshot: SessionSnapshot | null;
	delegations: Delegation[];
	hasMoreDelegations?: boolean;
	delegationsLoading: boolean;
	delegationsError: string | null;
	showAllDelegations?: boolean;
	onToggleShowAllDelegations?: () => void;
	runBoard: Omit<RunBoardCallbacks, "onSelectSession">;
	tools: ToolListing[];
	onSelectSession?: (sessionId: string) => void;
	onClose?: () => void;
}) {
	const [activeTab, setActiveTab] = useState<InspectorTab>("run-board");
	return (
		<div className="inspector-inner">
			<div className="inspector-tabs" role="tablist" aria-label="inspector tabs">
				{INSPECTOR_TABS.map((tab) => (
					<button
						key={tab.id}
						className={`inspector-tab ${activeTab === tab.id ? "active" : ""}`}
						type="button"
						role="tab"
						id={`inspector-tab-${tab.id}`}
						aria-selected={activeTab === tab.id}
						aria-controls={`inspector-panel-${tab.id}`}
						onClick={() => setActiveTab(tab.id)}
					>
						{tab.label}
					</button>
				))}
				<button className="plain-close-button inspector-close" type="button" onClick={onClose} aria-label="close inspector">
					<X size={14} />
				</button>
			</div>
			{activeTab === "run-board" ? (
				<div
					className="inspector-tab-panel"
					role="tabpanel"
					id="inspector-panel-run-board"
					aria-labelledby="inspector-tab-run-board"
				>
					<RunBoard
						parentSessionId={snapshot?.session_id ?? null}
						delegations={delegations}
						hasMoreDelegations={hasMoreDelegations}
						loading={delegationsLoading}
						error={delegationsError}
						showAllDelegations={showAllDelegations}
						onToggleShowAllDelegations={onToggleShowAllDelegations}
						onSelectSession={onSelectSession}
						onCancelDelegation={runBoard.onCancelDelegation}
						onReRunDelegation={runBoard.onReRunDelegation}
					/>
				</div>
			) : (
				<div
					className="inspector-tab-panel"
					role="tabpanel"
					id="inspector-panel-debug"
					aria-labelledby="inspector-tab-debug"
				>
					<section className="inspect-section">
						<h2>Session</h2>
						{snapshot ? (
							<>
								<div className="kv">
									<span>activity</span>
									<strong>{snapshot.activity}</strong>
								</div>
								<div className="kv">
									<span>archived</span>
									<strong>{snapshot.metadata.archived === true ? "yes" : "no"}</strong>
								</div>
								<div className="kv">
									<span>parent</span>
									{snapshot.parent_session_id ? (
										<button
											className="link-button"
											type="button"
											onClick={() => onSelectSession?.(snapshot.parent_session_id!)}
											title={`open parent ${snapshot.parent_session_id}`}
										>
											{snapshot.parent_session_id.slice(0, 13)}
										</button>
									) : (
										<strong>none</strong>
									)}
								</div>
								<div className="kv">
									<span>leaf</span>
									<strong>{snapshot.active_leaf_id?.slice(0, 12) ?? "root"}</strong>
								</div>
								<div className="kv">
									<span>metadata</span>
									<strong>{Object.keys(snapshot.metadata).length}</strong>
								</div>
							</>
						) : null}
					</section>
					<section className="inspect-section">
						<h2>Pending</h2>
						{snapshot?.pending_actions.length ? (
							<div className="pending-list">
								{snapshot.pending_actions.map((action) => (
									<div className="pending-row" key={action.action_row_id}>
										<span>{pendingActionLabel(action)}</span>
										<code>{action.action_row_id.slice(0, 12)}</code>
									</div>
								))}
							</div>
						) : (
							<p className="muted">No active work.</p>
						)}
					</section>
					<section className="inspect-section">
						<h2>Tools</h2>
						<div className="tool-list">
							{tools.map((tool) => (
								<span key={`${tool.kind}:${tool.name}`} title={tool.description || tool.name}>{tool.name}</span>
							))}
						</div>
					</section>
					<section className="inspect-section commands">
						<h2>Slash</h2>
						{COMMANDS.map((command) => (
							<div className="command-row" key={command.name}>
								<code>/{command.name}</code>
								<span>{command.argumentHint ?? ""}</span>
							</div>
						))}
					</section>
				</div>
			)}
		</div>
	);
}
