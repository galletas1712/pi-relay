import {
	Archive,
	ArchiveRestore,
	Bot,
	Edit3,
	FileText,
	Folder,
	PanelRightOpen,
	Plus,
	RotateCcw,
	Search,
	Settings,
	Square,
	Trash2,
	X
} from "lucide-react";
import { memo, useEffect, useRef, useState } from "react";
import { COMMANDS } from "./slash.ts";
import {
	isArchivedSession,
	projectTitle,
	sessionDisplayActivity,
	sessionTitle,
	displayActivity,
	type SessionDisplayActivity,
	type SessionListItem
} from "./sessionList.ts";
import { truncate } from "./text.ts";
import {
	canReRunDelegation,
	isDelegationRunning,
	delegationStatusLabel,
} from "./delegationBoard.ts";
import type {
	HandoffFileName,
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
	readHandoffFile: (delegationId: string, subagentId: string | null, file: HandoffFileName) => Promise<string>;
}

interface RunBoardProps extends RunBoardCallbacks {
	parentSessionId: string | null;
	delegations: Delegation[];
	loading: boolean;
	error: string | null;
}

const RUN_BOARD_DEFAULT_DELEGATION_COUNT = 3;

type OpenHandoffFile = { key: string; content: string } | { key: string; error: string };

function cancellationTranscriptFile(delegation: Delegation, subagent: DelegationSubagent): HandoffFileName | null {
	if (delegation.status !== "cancelled") return null;
	const file = subagent.transcript_file;
	if (typeof file !== "string") return null;
	if (!/^cancelled\/[^/]+\.transcript\.md$/.test(file)) return null;
	return file as HandoffFileName;
}

function compactText(value: string | null | undefined): string | null {
	if (typeof value !== "string") return null;
	const trimmed = value.trim();
	return trimmed.length > 0 ? trimmed : null;
}

function progressSummary(delegation: Delegation): string | null {
	const progress = delegation.progress;
	if (!progress) return null;
	const parts = [
		`${progress.terminal}/${progress.expected} terminal`,
		`${progress.running} running`,
		`${progress.failed} failed`,
	];
	return parts.join(", ");
}

function subagentStatusLabel(subagent: DelegationSubagent): string {
	const status = typeof subagent.status === "string" ? subagent.status : "idle";
	if (status === "done_with_failures") return "done with failures";
	return status.replaceAll("_", " ");
}

function SubagentRow({
	delegation,
	subagent,
	detailsOpen,
	open,
	onOpenFile,
	onCloseFile,
	onSelectSession,
}: {
	delegation: Delegation;
	subagent: DelegationSubagent;
	detailsOpen: boolean;
	open: OpenHandoffFile | null;
	onOpenFile: (subagentId: string, file: HandoffFileName) => void;
	onCloseFile: () => void;
	onSelectSession?: (sessionId: string) => void;
}) {
	const finalKey = `${subagent.id}:final_message.md`;
	const transcriptKey = `${subagent.id}:transcript.md`;
	const taskPromptKey = `${subagent.id}:task_prompt.md`;
	const cancellationFile = cancellationTranscriptFile(delegation, subagent);
	const cancellationKey = cancellationFile ? `${subagent.id}:${cancellationFile}` : null;
	const openFinal = open && open.key === finalKey ? open : null;
	const openTranscript = open && open.key === transcriptKey ? open : null;
	const openTaskPrompt = open && open.key === taskPromptKey ? open : null;
	const openCancellation = open && cancellationKey && open.key === cancellationKey ? open : null;
	const finalMessageFile = compactText(subagent.final_message_file);
	const transcriptFile = compactText(subagent.transcript_file);
	const taskPromptFile = compactText(subagent.task_prompt_file);
	const suggestedNext = compactText(subagent.suggested_next);
	const liveActivity =
		subagent.activity ??
		(subagent.status === "idle" || subagent.status === "queued" || subagent.status === "running" ? subagent.status : "idle");
	return (
		<div className="run-board-subagent" role="listitem">
			<div className="run-board-subagent-head">
				<button
					className="link-button"
					type="button"
					onClick={() => onSelectSession?.(subagent.id)}
					title={`open ${subagent.id}`}
				>
					<Bot size={12} /> {subagent.role ?? subagent.id.slice(0, 13)}
				</button>
				<span className={`subagent-activity ${displayActivity(liveActivity)}`}>
					{displayActivity(liveActivity)}
				</span>
			</div>
			<div className="run-board-subagent-summary">
				<span>{subagentStatusLabel(subagent)}</span>
				{suggestedNext ? (
					<span title={`suggested_next: ${suggestedNext}`}>
						suggested next <strong>{suggestedNext}</strong>
					</span>
				) : null}
			</div>
			{detailsOpen ? (
				<div className="run-board-debug">
					<div className="run-board-debug-row">
						<span>session</span>
						<code>{subagent.id}</code>
					</div>
					{taskPromptFile ? (
						<div className="run-board-debug-row">
							<span>task prompt</span>
							<code>{taskPromptFile}</code>
						</div>
					) : null}
					{finalMessageFile ? (
						<div className="run-board-debug-row">
							<span>final message</span>
							<code>{finalMessageFile}</code>
						</div>
					) : null}
					{transcriptFile && !cancellationFile ? (
						<div className="run-board-debug-row">
							<span>transcript</span>
							<code>{transcriptFile}</code>
						</div>
					) : null}
					{cancellationFile ? (
						<div className="run-board-debug-row">
							<span>cancellation transcript</span>
							<code>{cancellationFile}</code>
						</div>
					) : null}
				</div>
			) : null}
			{detailsOpen ? (
				<div className="run-board-handoff-links">
					{taskPromptFile ? (
						<button
							className="chip-button"
							type="button"
							onClick={() => (openTaskPrompt ? onCloseFile() : onOpenFile(subagent.id, "task_prompt.md"))}
							title="show task_prompt.md"
						>
							<FileText size={11} /> task prompt
						</button>
					) : null}
					{finalMessageFile ? (
						<button
							className="chip-button"
							type="button"
							onClick={() => (openFinal ? onCloseFile() : onOpenFile(subagent.id, "final_message.md"))}
							title="show final_message.md"
						>
							<FileText size={11} /> final message
						</button>
					) : null}
					{transcriptFile && !cancellationFile ? (
						<button
							className="chip-button"
							type="button"
							onClick={() => (openTranscript ? onCloseFile() : onOpenFile(subagent.id, "transcript.md"))}
							title="show transcript.md"
						>
							<FileText size={11} /> transcript
						</button>
					) : null}
					{cancellationFile ? (
						<button
							className="chip-button"
							type="button"
							onClick={() => (openCancellation ? onCloseFile() : onOpenFile(subagent.id, cancellationFile))}
							title="show cancellation transcript artifact"
						>
							<FileText size={11} /> cancellation transcript
						</button>
					) : null}
				</div>
			) : null}
			{openFinal ? <HandoffFileView open={openFinal} /> : null}
			{openTranscript ? <HandoffFileView open={openTranscript} /> : null}
			{openTaskPrompt ? <HandoffFileView open={openTaskPrompt} /> : null}
			{openCancellation ? <HandoffFileView open={openCancellation} /> : null}
		</div>
	);
}

function HandoffFileView({ open }: { open: OpenHandoffFile }) {
	if ("error" in open) return <p className="error-text run-board-file">{open.error}</p>;
	return <pre className="run-board-file">{open.content}</pre>;
}

function DelegationCard({
	delegation,
	canReRun,
	detailsOpen,
	open,
	onToggleDetails,
	onOpenFile,
	onCloseFile,
	onSelectSession,
	onCancelDelegation,
	onReRunDelegation,
}: {
	delegation: Delegation;
	canReRun: boolean;
	detailsOpen: boolean;
	open: OpenHandoffFile | null;
	onToggleDetails: () => void;
	onOpenFile: (subagentId: string | null, file: HandoffFileName) => void;
	onCloseFile: () => void;
} & Pick<RunBoardCallbacks, "onSelectSession" | "onCancelDelegation" | "onReRunDelegation">) {
	const running = isDelegationRunning(delegation);
	const title = delegation.label ?? delegation.workflow ?? delegation.delegation_id.slice(0, 13);
	const progress = progressSummary(delegation);
	return (
		<div className="run-board-delegation">
			<div className="run-board-delegation-head">
				<span className={`status-rail ${running ? "running" : "idle"}`} />
				<span className="run-board-delegation-title">
					{title}
					<span className="run-board-delegation-kind">{delegation.kind === "full" ? "full" : "fan-out"}</span>
				</span>
				<span className={`subagent-activity ${running ? "running" : "idle"}`}>{delegationStatusLabel(delegation.status)}</span>
			</div>
			{progress ? <div className="run-board-progress">{progress}</div> : null}
			{detailsOpen && delegation.handoff_dir ? (
				<div className="run-board-handoff-path" title={delegation.handoff_dir}>
					handoff {delegation.handoff_dir}
				</div>
			) : null}
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
				<button
					className={`chip-button ${detailsOpen ? "pressed" : ""}`}
					type="button"
					onClick={onToggleDetails}
					aria-expanded={detailsOpen}
					title={detailsOpen ? "hide delegation artifact details" : "show delegation artifact details"}
				>
					<FileText size={11} /> {detailsOpen ? "hide details" : "details"}
				</button>
			</div>
			<div className="run-board-subagents" role="list">
				{delegation.subagents.map((subagent) => (
					<SubagentRow
						key={subagent.id}
						delegation={delegation}
						subagent={subagent}
						detailsOpen={detailsOpen}
						open={open}
						onOpenFile={onOpenFile}
						onCloseFile={onCloseFile}
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
	openDebugDelegationIds,
	openFile,
	onToggleShowAllDelegations,
	onToggleDelegationDebug,
	onOpenFile,
	onCloseFile,
	onSelectSession,
	onCancelDelegation,
	onReRunDelegation,
}: {
	delegations: Delegation[];
	showAllDelegations: boolean;
	openDebugDelegationIds: ReadonlySet<string>;
	openFile: { delegationId: string; open: OpenHandoffFile } | null;
	onToggleShowAllDelegations: () => void;
	onToggleDelegationDebug: (delegationId: string) => void;
	onOpenFile: (delegationId: string, subagentId: string | null, file: HandoffFileName) => void;
	onCloseFile: () => void;
} & Pick<RunBoardCallbacks, "onSelectSession" | "onCancelDelegation" | "onReRunDelegation">) {
	const hiddenDelegationCount = Math.max(0, delegations.length - RUN_BOARD_DEFAULT_DELEGATION_COUNT);
	const visibleDelegations =
		showAllDelegations || hiddenDelegationCount === 0
			? delegations
			: delegations.slice(0, RUN_BOARD_DEFAULT_DELEGATION_COUNT);
	return (
		<div className="run-board">
			{visibleDelegations.map((delegation) => (
				<DelegationCard
					key={delegation.delegation_id}
					delegation={delegation}
					canReRun={canReRunDelegation(delegation)}
					detailsOpen={openDebugDelegationIds.has(delegation.delegation_id)}
					open={openFile && openFile.delegationId === delegation.delegation_id ? openFile.open : null}
					onToggleDetails={() => onToggleDelegationDebug(delegation.delegation_id)}
					onOpenFile={(subagentId, file) => onOpenFile(delegation.delegation_id, subagentId, file)}
					onCloseFile={onCloseFile}
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
	readHandoffFile,
}: RunBoardProps) {
	// Which handoff file (if any) is currently expanded. Keyed by
	// `${subagentId ?? "delegation"}:${file}` so only one file is open at a time.
	const [openFile, setOpenFile] = useState<{ delegationId: string; open: OpenHandoffFile } | null>(null);
	const [showAllDelegations, setShowAllDelegations] = useState(false);
	const [openDebugDelegationIds, setOpenDebugDelegationIds] = useState<Set<string>>(() => new Set());

	const openHandoffFile = (delegationId: string, subagentId: string | null, file: HandoffFileName) => {
		const key = `${subagentId ?? "delegation"}:${file}`;
		setOpenFile({ delegationId, open: { key, content: "" } });
		void readHandoffFile(delegationId, subagentId, file)
			.then((content) => setOpenFile({ delegationId, open: { key, content } }))
			.catch((cause: unknown) =>
				setOpenFile({ delegationId, open: { key, error: cause instanceof Error ? cause.message : String(cause) } }),
			);
	};
	const toggleDelegationDebug = (delegationId: string) => {
		const closing = openDebugDelegationIds.has(delegationId);
		setOpenDebugDelegationIds((current) => {
			const next = new Set(current);
			if (next.has(delegationId)) {
				next.delete(delegationId);
			} else {
				next.add(delegationId);
			}
			return next;
		});
		if (closing && openFile?.delegationId === delegationId) setOpenFile(null);
	};

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
					openDebugDelegationIds={openDebugDelegationIds}
					openFile={openFile}
					onToggleShowAllDelegations={() => setShowAllDelegations((current) => !current)}
					onToggleDelegationDebug={toggleDelegationDebug}
					onOpenFile={openHandoffFile}
					onCloseFile={() => setOpenFile(null)}
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
						placeholder="Filter Sessions..."
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
	const displayActivity = sessionDisplayActivity(session);
	const canArchive = session.activity === "idle";
	const canDelete = session.activity === "idle";
	const ArchiveIcon = archived ? ArchiveRestore : Archive;
	return (
		<button className={`session-row ${selected ? "selected" : ""} ${archived ? "archived" : ""}`} type="button" onClick={onSelect}>
			<span className={`status-rail ${archived ? "archived" : displayActivity}`} />
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
					title={canArchive ? (archived ? "unarchive session" : "archive session") : "only idle sessions can be archived"}
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
					title={canDelete ? "delete session" : "only idle sessions can be deleted"}
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
	activity,
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
	activity: SessionDisplayActivity | null;
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
				<span className={`session-state ${archived ? "archived" : activity ?? "idle"}`}>
					{archived ? "archived" : activity}
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
				readHandoffFile={runBoard.readHandoffFile}
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
