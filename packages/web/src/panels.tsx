import {
	Archive,
	ArchiveRestore,
	Bot,
	Edit3,
	Folder,
	Network,
	PanelRightOpen,
	Plus,
	RotateCcw,
	Search,
	Settings,
	Square,
	SquarePen,
	Trash2,
	X
} from "lucide-react";
import { memo, useEffect, useRef, useState } from "react";
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
	loading: boolean;
	error: string | null;
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
						<Bot size={12} aria-hidden />
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
					<KindIcon size={14} aria-hidden />
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
	showAllDelegations,
	onToggleShowAllDelegations,
	onSelectSession,
	onCancelDelegation,
	onReRunDelegation,
}: {
	delegations: Delegation[];
	showAllDelegations: boolean;
	onToggleShowAllDelegations: () => void;
} & Pick<RunBoardCallbacks, "onSelectSession" | "onCancelDelegation" | "onReRunDelegation">) {
	// The daemon returns delegations oldest-first (ORDER BY created_at, id); show
	// them newest-first so the most recently launched delegation is on top and
	// the "see more" collapse hides the oldest.
	const orderedDelegations = [...delegations].reverse();
	const hiddenDelegationCount = Math.max(0, orderedDelegations.length - RUN_BOARD_DEFAULT_DELEGATION_COUNT);
	const visibleDelegations =
		showAllDelegations || hiddenDelegationCount === 0
			? orderedDelegations
			: orderedDelegations.slice(0, RUN_BOARD_DEFAULT_DELEGATION_COUNT);
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
			{hiddenDelegationCount > 0 ? (
				<button className="chip-button run-board-toggle" type="button" onClick={onToggleShowAllDelegations}>
					{showAllDelegations ? "show fewer" : `see more (${hiddenDelegationCount})`}
				</button>
			) : null}
		</div>
	);
}

function RunBoard({
	parentSessionId,
	delegations,
	loading,
	error,
	onSelectSession,
	onCancelDelegation,
	onReRunDelegation,
}: RunBoardProps) {
	const [showAllDelegations, setShowAllDelegations] = useState(false);

	useEffect(() => {
		setShowAllDelegations(false);
	}, [parentSessionId]);

	return (
		<section className="inspect-section">
			<h2>Run board</h2>
			{!parentSessionId ? <p className="muted">No session selected.</p> : null}
			{loading ? <p className="muted">Loading delegations…</p> : null}
			{error ? <p className="error-text">{error}</p> : null}
			{parentSessionId && !loading && !error && delegations.length === 0 ? <p className="muted">No delegations yet.</p> : null}
			{parentSessionId && delegations.length > 0 ? (
				<RunBoardDelegationList
					delegations={delegations}
					showAllDelegations={showAllDelegations}
					onToggleShowAllDelegations={() => setShowAllDelegations((current) => !current)}
					onSelectSession={onSelectSession}
					onCancelDelegation={onCancelDelegation}
					onReRunDelegation={onReRunDelegation}
				/>
			) : null}
		</section>
	);
}

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
	inert?: boolean;
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
	inert,
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
			/>
			<div className="session-list" role="listbox" aria-label="sessions">
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
				{filteredSessions.length === 0 ? <div className="empty-list">No sessions</div> : null}
			</div>
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
			<div className="project-list" role="listbox" aria-label="projects">
				<button
					className={`project-row ${selectedProjectId === null ? "selected" : ""}`}
					type="button"
					onClick={() => onSelectProject(null)}
					title="Ephemeral host sessions start from your home directory"
				>
					<Folder size={14} />
					<span className="project-main">
						<span className="project-title">Host</span>
						<span className="project-cwd">Ephemeral sessions</span>
					</span>
				</button>
				{projects.map((project) => (
					<button
						className={`project-row ${project.project_id === selectedProjectId ? "selected" : ""}`}
							type="button"
							key={project.project_id}
							onClick={() => onSelectProject(project.project_id)}
							title={projectWorkspaceSummary(project)}
						>
						<Folder size={14} />
						<span className="project-main">
								<span className="project-title">{projectTitle(project)}</span>
								<span className="project-cwd">{project.workspaces.length} workspaces</span>
						</span>
						<span
							className="session-row-action"
							role="button"
							tabIndex={0}
							title="edit project"
							aria-label={`edit ${projectTitle(project)}`}
							onClick={(event) => {
								event.stopPropagation();
								onEditProject(project);
							}}
							onKeyDown={(event) => {
								if (event.key !== "Enter" && event.key !== " ") return;
								event.preventDefault();
								event.stopPropagation();
								onEditProject(project);
							}}
						>
							<Edit3 size={13} />
						</span>
					</button>
				))}

			</div>
		</div>
	);
}

export function SidebarToolbar({
	disabled,
	query,
	onQueryChange,
	showArchived,
	onToggleArchived,
	onNew
}: {
	disabled: boolean;
	query: string;
	onQueryChange: (query: string) => void;
	showArchived: boolean;
	onToggleArchived: () => void;
	onNew: () => void;
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
				<button className="primary-button" type="button" onClick={onNew} disabled={disabled}>
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
	const ArchiveIcon = archived ? ArchiveRestore : Archive;
	return (
		<button className={`session-row ${selected ? "selected" : ""} ${archived ? "archived" : ""}`} type="button" onClick={onSelect}>
			<span className={`status-rail ${archived ? "archived" : status}`} />
			<span className="session-main">
				<span className="session-title">{sessionTitle(session)}</span>
				<span className="session-sub">
					{archived ? "archived - " : ""}{session.provider.model}
				</span>
				<span className="session-leaf">
					{session.active_leaf_id ? session.active_leaf_id.slice(0, 6) : "root"}
				</span>
			</span>
			<span className="session-row-actions" aria-label="session actions">
				<span
					className="session-row-action"
					role="button"
					tabIndex={0}
					title="rename session"
					aria-label={`rename ${sessionTitle(session)}`}
					onClick={(event) => {
						event.stopPropagation();
						onRename();
					}}
					onKeyDown={(event) => {
						if (event.key !== "Enter" && event.key !== " ") return;
						event.preventDefault();
						event.stopPropagation();
						onRename();
					}}
				>
					<Edit3 size={13} />
				</span>
				<span
					className={`session-row-action ${canArchive ? "" : "disabled"}`}
					role="button"
					tabIndex={canArchive ? 0 : -1}
					title={canArchive ? (archived ? "unarchive session" : "archive session") : "only idle sessions with no running subagents can be archived"}
					aria-label={`${archived ? "unarchive" : "archive"} ${sessionTitle(session)}`}
					aria-disabled={!canArchive}
					onClick={(event) => {
						event.stopPropagation();
						if (canArchive) onArchiveToggle();
					}}
					onKeyDown={(event) => {
						if (!canArchive || (event.key !== "Enter" && event.key !== " ")) return;
						event.preventDefault();
						event.stopPropagation();
						onArchiveToggle();
					}}
				>
					<ArchiveIcon size={13} />
				</span>
				<span
					className={`session-row-action danger ${canDelete ? "" : "disabled"}`}
					role="button"
					tabIndex={canDelete ? 0 : -1}
					title={canDelete ? "delete session" : "only idle sessions with no running subagents can be deleted"}
					aria-label={`delete ${sessionTitle(session)}`}
					aria-disabled={!canDelete}
					onClick={(event) => {
						event.stopPropagation();
						if (canDelete) onDelete();
					}}
					onKeyDown={(event) => {
						if (!canDelete || (event.key !== "Enter" && event.key !== " ")) return;
						event.preventDefault();
						event.stopPropagation();
						onDelete();
					}}
				>
					<Trash2 size={13} />
				</span>
			</span>
		</button>
	);
}

export function LogHeader({
	archived,
	status,
	title,
	modelOptions,
	modelValue,
	modelDisabled,
	modelDisabledTitle,
	reasoningEfforts,
	reasoningEffort,
	onModelChange,
	onReasoningEffortChange,
	rightOpen,
	onToggleRight
}: {
	archived: boolean;
	status: SessionStatus | null;
	title: string | null;
	modelOptions: { id: string; label: string }[];
	modelValue: string;
	modelDisabled: boolean;
	modelDisabledTitle: string;
	reasoningEfforts: ReasoningEffort[];
	reasoningEffort: ReasoningEffort;
	onModelChange: (value: string) => void;
	onReasoningEffortChange: (value: ReasoningEffort) => void;
	rightOpen: boolean;
	onToggleRight: () => void;
}) {
	return (
		<div className="log-header">
			{title ? (
				<span className={`session-state ${archived ? "archived" : status ?? "idle"}`}>
					{archived ? "archived" : status}
				</span>
			) : null}
			{title ? (
				<span className="log-session">
					{title}
				</span>
			) : (
				<span className="log-session">No session selected</span>
			)}
			<div className="log-controls">
				<label className="header-select">
					<span>model</span>
					<select
						value={modelValue}
						disabled={modelDisabled}
						title={modelDisabledTitle}
						onChange={(event) => onModelChange(event.target.value)}
					>
						{modelOptions.map((option) => (
							<option key={option.id} value={option.id}>{option.label}</option>
						))}
					</select>
				</label>
				<label className="header-select compact">
					<span>effort</span>
					<select
						value={reasoningEffort}
						title="reasoning effort"
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
	delegationsLoading,
	delegationsError,
	runBoard,
	tools,
	onSelectSession,
	onClose
}: {
	snapshot: SessionSnapshot | null;
	delegations: Delegation[];
	delegationsLoading: boolean;
	delegationsError: string | null;
	runBoard: Omit<RunBoardCallbacks, "onSelectSession">;
	tools: ToolListing[];
	onSelectSession?: (sessionId: string) => void;
	onClose?: () => void;
}) {
	return (
		<div className="inspector-inner">
			<div className="inspector-head">
				<Settings size={14} />
				<span>inspector</span>
				<button className="plain-close-button inspector-close" type="button" onClick={onClose} aria-label="close inspector">
					<X size={14} />
				</button>
			</div>
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
				) : (
					<p className="muted">No session selected.</p>
				)}
			</section>
			<RunBoard
				parentSessionId={snapshot?.session_id ?? null}
				delegations={delegations}
				loading={delegationsLoading}
				error={delegationsError}
				onSelectSession={onSelectSession}
				onCancelDelegation={runBoard.onCancelDelegation}
				onReRunDelegation={runBoard.onReRunDelegation}
			/>
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
	);
}
