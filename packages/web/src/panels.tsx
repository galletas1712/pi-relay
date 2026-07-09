import {
	Archive,
	ArchiveRestore,
	ArrowUp,
	Ban,
	Bot,
	CircleCheck,
	CircleAlert,
	CircleDashed,
	CircleHelp,
	CircleX,
	Clock3,
	Folder,
	Loader2,
	PanelRightOpen,
	Plus,
	Search,
	Square,
	SquarePen,
	TriangleAlert,
	Trash2,
	X
} from "lucide-react";
import { memo, useEffect, useMemo, useRef, useState, type RefObject } from "react";
import { ActionMenu, type ActionMenuItem } from "./actionMenu.tsx";
import { ConnectionBlockedReason, firstDisabledReason } from "./connectionRecovery.tsx";
import {
	AppAlertDialog,
	DialogBody,
	DialogClose,
	DialogCloseButton,
	DialogDescription,
	DialogFooter,
	DialogHeader,
	DialogHeading,
	DialogTitle,
} from "./dialog.tsx";
import { COMMANDS } from "./slash.ts";
import {
	isArchivedSession,
	projectTitle,
	sessionStatusWithDelegations,
	sessionTitle,
	type SessionStatus,
	type SessionListItem
} from "./sessionList.ts";
import {
	agentStatusIconKey,
	isDelegationRunning,
	orderDelegations,
	remainingDelegationWorkCount,
	statusIconClass,
	type AgentStatusIconKey,
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

export const RUN_BOARD_DEFAULT_DELEGATION_COUNT = 3;
export const RUN_BOARD_EXPANDED_DELEGATION_COUNT = 100;
const EMPTY_SUBAGENT_NAMES = new Map<string, string>();

export function subagentStatusLabel(subagent: DelegationSubagent): string {
	const status = typeof subagent.status === "string" ? subagent.status : "idle";
	if (status === "done_with_failures") return "done with failures";
	return status.replaceAll("_", " ");
}

function AgentStatusIcon({ status }: { status: string }) {
	const label = status === "done_with_failures" ? "done with failures" : status.replaceAll("_", " ");
	const iconKey = agentStatusIconKey(status);
	const icons = {
		running: Loader2,
		done: CircleCheck,
		"done-with-failures": TriangleAlert,
		failed: CircleX,
		cancelled: Ban,
		queued: Clock3,
		idle: CircleDashed,
		unknown: CircleHelp,
	} satisfies Record<AgentStatusIconKey, typeof Loader2>;
	const Icon = icons[iconKey];
	return (
		<span
			className={`run-board-status-icon ${statusIconClass(status)}`}
			data-status-icon={iconKey}
			role="img"
			aria-label={`${label} status`}
			title={label}
		>
			<Icon size={16} aria-hidden />
		</span>
	);
}

function SubagentRow({
	subagent,
	displayName,
	selected,
	onSelectSession,
}: {
	subagent: DelegationSubagent;
	displayName: string;
	selected: boolean;
	onSelectSession?: (sessionId: string) => void;
}) {
	const status = typeof subagent.status === "string" ? subagent.status : "idle";
	const statusLabel = subagentStatusLabel(subagent);
	const role = subagent.role?.trim() || null;
	const accessibleRole = role ? `, ${role}` : "";
	return (
		<div className="run-board-subagent" role="listitem">
			<button
				className="run-board-subagent-button"
				type="button"
				onClick={() => onSelectSession?.(subagent.id)}
				aria-current={selected ? "page" : undefined}
				aria-label={`Open agent ${displayName}${accessibleRole}, ${statusLabel}`}
			>
				<span className="run-board-subagent-main">
					<AgentStatusIcon status={status} />
					<span className="run-board-subagent-copy">
						<span className="run-board-subagent-name">{displayName}</span>
						{role ? <span className="run-board-subagent-role">{role}</span> : null}
					</span>
				</span>
			</button>
		</div>
	);
}

interface DelegationActionState {
	pending: boolean;
	error: string | null;
}

function actionErrorMessage(error: unknown): string {
	return error instanceof Error ? error.message : String(error);
}

function DelegationCard({
	delegation,
	subagentNames,
	selectedSessionId,
	actionState,
	onSelectSession,
	onRequestCancel,
	mutationBlockedReason,
}: {
	delegation: Delegation;
	subagentNames: ReadonlyMap<string, string>;
	selectedSessionId?: string | null;
	actionState?: DelegationActionState;
	onRequestCancel: (delegation: Delegation) => void;
	mutationBlockedReason?: string | null;
	onSelectSession?: (sessionId: string) => void;
}) {
	const running = isDelegationRunning(delegation);
	const title = delegation.label?.trim() || "Agent task";
	const statusLabel = delegation.status === "done_with_failures"
		? "done with failures"
		: delegation.status.replaceAll("_", " ");
	const pending = actionState?.pending ?? false;
	const actionDisabled = pending || !!mutationBlockedReason;
	return (
		<article className="run-board-delegation" aria-label={`${title}, ${statusLabel}`}>
			<div className="run-board-delegation-head">
				<AgentStatusIcon status={delegation.status} />
				<strong className="run-board-delegation-title">{title}</strong>
				<div className="run-board-delegation-controls">
					{running ? (
						<button
							className="run-board-cancel"
							type="button"
							disabled={actionDisabled}
							aria-busy={pending}
							aria-label={pending ? "Cancelling…" : "Cancel"}
							onClick={() => onRequestCancel(delegation)}
							title="Cancel this delegated work"
						>
							{pending ? <Loader2 className="spin" size={15} aria-hidden /> : <Square size={13} aria-hidden />}
						</button>
					) : null}
				</div>
				{running ? <ConnectionBlockedReason reason={mutationBlockedReason} className="run-board-blocked-reason" /> : null}
			</div>
			{actionState?.error ? (
				<p className="run-board-action-error" role="alert">
					<CircleAlert size={13} aria-hidden />
					{actionState.error}
				</p>
			) : null}
			<div className="run-board-subagents" role="list">
				{delegation.subagents.map((subagent) => (
					<SubagentRow
						key={subagent.id}
						subagent={subagent}
						displayName={subagentNames.get(subagent.id) ?? "Agent"}
						selected={subagent.id === selectedSessionId}
						onSelectSession={onSelectSession}
					/>
				))}
			</div>
		</article>
	);
}

function CancelDelegationDialog({
	delegation,
	busy,
	blockedReason,
	onClose,
	onConfirm,
}: {
	delegation: Delegation;
	busy: boolean;
	blockedReason?: string | null;
	onClose: () => void;
	onConfirm: () => void;
}) {
	const cancelRef = useRef<HTMLButtonElement>(null);
	const title = delegation.label?.trim() || "Agent task";
	const remaining = remainingDelegationWorkCount(delegation);
	return (
		<AppAlertDialog
			className="rename-dialog cancel-delegation-dialog"
			busy={busy}
			initialFocusRef={cancelRef}
			onDismiss={onClose}
		>
			<DialogHeader>
				<DialogHeading>
					<DialogTitle>Cancel delegated work?</DialogTitle>
				</DialogHeading>
				<DialogCloseButton label="close cancel delegated work dialog" disabled={busy} />
			</DialogHeader>
			<DialogBody className="delete-dialog-body">
				<p>
					Cancel <strong>{title}</strong> and stop remaining work affecting {remaining.count} {remaining.unit}?
				</p>
				<DialogDescription className="muted">
					This stops remaining delegated work. It cannot roll back external tool or network side effects that already happened.
				</DialogDescription>
				<ConnectionBlockedReason reason={blockedReason} />
			</DialogBody>
			<DialogFooter>
				<DialogClose ref={cancelRef} className="secondary-button" disabled={busy}>
					Cancel
				</DialogClose>
				<button
					type="button"
					className="primary-button destructive"
					disabled={busy || !!blockedReason}
					aria-busy={busy}
					onClick={onConfirm}
				>
					{busy ? "Cancelling…" : "Cancel work"}
				</button>
			</DialogFooter>
		</AppAlertDialog>
	);
}

export function RunBoardDelegationList({
	parentSessionId,
	delegations,
	subagentNames = EMPTY_SUBAGENT_NAMES,
	hasMoreDelegations = false,
	showAllDelegations,
	onToggleShowAllDelegations,
	selectedSessionId,
	onSelectSession,
	onCancelDelegation,
	mutationBlockedReason,
	remoteReadBlockedReason,
	expandedDelegationsAvailable = false,
	boundedExpansionHasMore = false,
}: {
	parentSessionId: string;
	delegations: Delegation[];
	subagentNames?: ReadonlyMap<string, string>;
	hasMoreDelegations?: boolean;
	showAllDelegations: boolean;
	onToggleShowAllDelegations: () => void;
	selectedSessionId?: string | null;
	mutationBlockedReason?: string | null;
	remoteReadBlockedReason?: string | null;
	expandedDelegationsAvailable?: boolean;
	boundedExpansionHasMore?: boolean;
	onSelectSession?: (sessionId: string) => void;
	onCancelDelegation: (parentSessionId: string, delegationId: string) => void | Promise<void>;
}) {
	const [cancelDialogIntent, setCancelDialogIntent] = useState<{
		parentSessionId: string;
		delegation: Delegation;
	} | null>(null);
	const [actionStates, setActionStates] = useState<Record<string, DelegationActionState>>({});
	const actionLocks = useRef(new Set<string>());
	// The daemon returns a bounded newest-first page for the Agents outline. Keep
	// a local cap as a defensive fallback when tests or cached data include extras.
	const hiddenLocalCount = Math.max(0, delegations.length - RUN_BOARD_DEFAULT_DELEGATION_COUNT);
	const visibleDelegations =
		showAllDelegations || hiddenLocalCount === 0
			? delegations
			: delegations.slice(0, RUN_BOARD_DEFAULT_DELEGATION_COUNT);
	const orderedDelegations = useMemo(
		() => orderDelegations(visibleDelegations),
		[visibleDelegations],
	);
	const showToggle = hasMoreDelegations || hiddenLocalCount > 0 || showAllDelegations;
	const toggleBlockedReason =
		!showAllDelegations &&
		hiddenLocalCount === 0 &&
		hasMoreDelegations &&
		!expandedDelegationsAvailable
			? remoteReadBlockedReason
			: null;
	const actionKey = (intentParentSessionId: string, delegationId: string) =>
		`${intentParentSessionId}:${delegationId}`;
	const setActionState = (key: string, state: DelegationActionState) => {
		setActionStates((current) => ({ ...current, [key]: state }));
	};
	const runAction = async (
		intentParentSessionId: string,
		delegation: Delegation,
		callback: () => void | Promise<void>,
	) => {
		const delegationId = delegation.delegation_id;
		const key = actionKey(intentParentSessionId, delegationId);
		if (actionLocks.current.has(key)) return null;
		actionLocks.current.add(key);
		setActionState(key, { pending: true, error: null });
		try {
			await callback();
			setActionState(key, { pending: false, error: null });
			return true;
		} catch (error) {
			setActionState(key, { pending: false, error: actionErrorMessage(error) });
			return false;
		} finally {
			actionLocks.current.delete(key);
		}
	};
	const confirmCancel = () => {
		const intent = cancelDialogIntent;
		if (!intent || mutationBlockedReason) return;
		void runAction(
			intent.parentSessionId,
			intent.delegation,
			() => onCancelDelegation(intent.parentSessionId, intent.delegation.delegation_id),
		).then((settled) => {
			if (settled !== null) setCancelDialogIntent(null);
		});
	};
	return (
		<div className="run-board">
			{parentSessionId && delegations.length > 0 ? (
				<>
					<div className="run-board-outline">
						{orderedDelegations.map((delegation) => (
							<DelegationCard
								key={delegation.delegation_id}
								delegation={delegation}
								subagentNames={subagentNames}
								selectedSessionId={selectedSessionId}
								actionState={actionStates[actionKey(parentSessionId, delegation.delegation_id)]}
								onSelectSession={onSelectSession}
								onRequestCancel={(selectedDelegation) =>
									setCancelDialogIntent({
										parentSessionId,
										delegation: selectedDelegation,
									})}
								mutationBlockedReason={mutationBlockedReason}
							/>
						))}
					</div>
					{showToggle ? (
						<>
							<button
								className="run-board-toggle"
								type="button"
								disabled={!!toggleBlockedReason}
								onClick={onToggleShowAllDelegations}
							>
								{showAllDelegations ? "Show fewer" : `See more${hiddenLocalCount > 0 ? ` (${hiddenLocalCount})` : ""}`}
							</button>
							<ConnectionBlockedReason reason={toggleBlockedReason} />
						</>
					) : null}
					{showAllDelegations && boundedExpansionHasMore ? (
						<p className="run-board-page-limit" role="status">
							Latest {RUN_BOARD_EXPANDED_DELEGATION_COUNT} shown.
						</p>
					) : null}
				</>
			) : null}
			{cancelDialogIntent ? (
				<CancelDelegationDialog
					delegation={cancelDialogIntent.delegation}
					busy={
						actionStates[
							actionKey(
								cancelDialogIntent.parentSessionId,
								cancelDialogIntent.delegation.delegation_id,
							)
						]?.pending === true
					}
					blockedReason={mutationBlockedReason}
					onClose={() => setCancelDialogIntent(null)}
					onConfirm={confirmCancel}
				/>
			) : null}
		</div>
	);
}

function RunBoard({
	parentSessionId,
	delegations,
	subagentNames,
	hasMoreDelegations,
	loading,
	error,
	showAllDelegations,
	onToggleShowAllDelegations,
	onRetryDelegations,
	delegationsRetrying = false,
	selectedSessionId,
	boundedExpansionHasMore = false,
	onSelectSession,
	onCancelDelegation,
	mutationBlockedReason,
	remoteReadBlockedReason,
	expandedDelegationsAvailable,
}: Omit<Parameters<typeof RunBoardDelegationList>[0], "parentSessionId"> & {
	parentSessionId: string | null;
	loading: boolean;
	error: string | null;
	onRetryDelegations?: () => void;
	delegationsRetrying?: boolean;
}) {
	return (
		<section className="inspect-section run-board-section">
			{parentSessionId && loading ? (
				<p className="muted run-board-inline-status" role="status">
					{delegations.length > 0 ? "Refreshing agents…" : "Loading agents…"}
				</p>
			) : null}
			{parentSessionId && error ? (
				<div className="load-error-banner run-board-load-error" role="alert">
					<div>
						<strong>{delegations.length > 0 ? "Agent refresh failed" : "Couldn’t load agents"}</strong>
						<span>{error}</span>
					</div>
					{onRetryDelegations ? (
						<>
							<button
								type="button"
								className="secondary-button load-error-retry"
								disabled={delegationsRetrying || !!remoteReadBlockedReason}
								aria-busy={delegationsRetrying}
								onClick={onRetryDelegations}
							>
								{delegationsRetrying ? "Retrying…" : "Retry"}
							</button>
							<ConnectionBlockedReason reason={remoteReadBlockedReason} />
						</>
					) : null}
				</div>
			) : null}
			{parentSessionId && !loading && !error && delegations.length === 0 ? <p className="muted">No delegated work yet.</p> : null}
			<RunBoardDelegationList
				parentSessionId={parentSessionId ?? ""}
				delegations={delegations}
				subagentNames={subagentNames}
				hasMoreDelegations={hasMoreDelegations}
				showAllDelegations={showAllDelegations}
				onToggleShowAllDelegations={onToggleShowAllDelegations}
				selectedSessionId={selectedSessionId}
				onSelectSession={onSelectSession}
				onCancelDelegation={onCancelDelegation}
				mutationBlockedReason={mutationBlockedReason}
				remoteReadBlockedReason={remoteReadBlockedReason}
				expandedDelegationsAvailable={expandedDelegationsAvailable}
				boundedExpansionHasMore={boundedExpansionHasMore}
			/>
		</section>
	);
}

type InspectorTab = "run-board" | "debug";

const INSPECTOR_TABS: { id: InspectorTab; label: string }[] = [
	{ id: "run-board", label: "Agents" },
	{ id: "debug", label: "Inspector" },
];

export interface SidebarProps {
	connection: string;
	projects: Project[];
	projectsLoading?: boolean;
	projectsFetching?: boolean;
	projectsError?: string | null;
	projectsHasCachedData?: boolean;
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
	onRetryProjects?: () => void;
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
	mutationBlockedReason?: string | null;
	remoteReadBlockedReason?: string | null;
}

export const Sidebar = memo(function Sidebar({
	connection,
	projects,
	projectsLoading = false,
	projectsFetching = false,
	projectsError = null,
	projectsHasCachedData = false,
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
	onRetryProjects,
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
	onDelete,
	mutationBlockedReason,
	remoteReadBlockedReason,
}: SidebarProps) {
	return (
		<aside className="sidebar" data-slot="sidebar" inert={inert}>
			<SidebarHeader connection={connection} onClose={onClose} />
			<ProjectList
				projects={projects}
				loading={projectsLoading}
				fetching={projectsFetching}
				error={projectsError}
				hasCachedData={projectsHasCachedData}
				remoteReadBlockedReason={remoteReadBlockedReason}
				selectedProjectId={selectedProjectId}
				onRetry={onRetryProjects}
				onSelectProject={onSelectProject}
				onNewProject={onNewProject}
				onEditProject={onEditProject}
			/>
			<div className="session-section-head">
				<span>Sessions</span>
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
						<>
							<button
								type="button"
								className="secondary-button load-error-retry"
								disabled={sessionsFetching || !!remoteReadBlockedReason}
								aria-busy={sessionsFetching}
								onClick={onRetrySessions}
							>
								{sessionsFetching ? "Retrying…" : "Retry"}
							</button>
							<ConnectionBlockedReason reason={remoteReadBlockedReason} />
						</>
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
							mutationBlockedReason={mutationBlockedReason}
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
	loading = false,
	fetching = false,
	error = null,
	hasCachedData = false,
	remoteReadBlockedReason,
	selectedProjectId,
	onRetry,
	onSelectProject,
	onNewProject,
	onEditProject
}: {
	projects: Project[];
	loading?: boolean;
	fetching?: boolean;
	error?: string | null;
	hasCachedData?: boolean;
	remoteReadBlockedReason?: string | null;
	selectedProjectId: string | null;
	onRetry?: () => void;
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
			{error ? (
				<div className="load-error-banner project-load-error" role="alert">
					<div>
						<strong>{hasCachedData ? "Project refresh failed" : "Couldn’t load projects"}</strong>
						<span>{error}</span>
					</div>
					{onRetry ? (
						<>
							<button
								type="button"
								className="secondary-button load-error-retry"
								disabled={fetching || !!remoteReadBlockedReason}
								aria-busy={fetching}
								onClick={onRetry}
							>
								{fetching ? "Retrying…" : "Retry"}
							</button>
							<ConnectionBlockedReason reason={remoteReadBlockedReason} />
						</>
					) : null}
				</div>
			) : null}
			<nav aria-label="Projects" aria-busy={loading || fetching}>
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
							<span className="project-title">Host</span>
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
									aria-current={selected ? "page" : undefined}
								>
									<span
										className="project-folder-count"
										role="img"
										aria-label={`${project.workspaces.length} ${project.workspaces.length === 1 ? "workspace" : "workspaces"}`}
										title={`${project.workspaces.length} ${project.workspaces.length === 1 ? "workspace" : "workspaces"}`}
									>
										<Folder size={18} aria-hidden />
										<span aria-hidden>{project.workspaces.length}</span>
									</span>
									<span className="project-title">{title}</span>
								</button>
								<ActionMenu
									triggerLabel={`Open project actions for ${title}`}
									items={projectMenuItems(project, onEditProject)}
								/>
							</li>
						);
					})}
					{loading && !error ? <li className="empty-list compact">Loading projects…</li> : null}
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

export function SessionRow({
	session,
	selected,
	onSelect,
	onRename,
	onArchiveToggle,
	onDelete,
	mutationBlockedReason,
}: {
	session: SessionListItem;
	selected: boolean;
	onSelect: () => void;
	onRename: () => void;
	onArchiveToggle: () => void;
	onDelete: () => void;
	mutationBlockedReason?: string | null;
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
					<span className="session-sub">{session.provider.model}</span>
				</span>
			</button>
			<ActionMenu
				triggerLabel={`Open session actions for ${title}`}
				items={sessionMenuItems({ archived, canArchive, canDelete, onRename, onArchiveToggle, onDelete, mutationBlockedReason })}
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
	mutationBlockedReason,
}: {
	archived: boolean;
	canArchive: boolean;
	canDelete: boolean;
	onRename: () => void;
	onArchiveToggle: () => void;
	onDelete: () => void;
	mutationBlockedReason?: string | null;
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
			disabled: !canArchive || !!mutationBlockedReason,
			disabledReason: firstDisabledReason(
				mutationBlockedReason,
				!canArchive && IDLE_SESSION_ACTION_REASON,
			) ?? undefined,
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
	modelLocked = false,
	reasoningDisabled = false,
	controlsBlockedReason,
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
	modelLocked?: boolean;
	reasoningDisabled?: boolean;
	controlsBlockedReason?: string | null;
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
				<label className="header-select" title={modelLocked ? "Model, locked" : "Model"}>
					<span className="sr-only">Model</span>
					<select
						value={modelValue}
						disabled={modelDisabled}
						title={modelLocked ? "Model, locked" : "Model"}
						aria-label={modelLocked ? "Model, locked" : "Model"}
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
						disabled={reasoningDisabled}
						title="Reasoning effort"
						aria-label="Reasoning effort"
						onChange={(event) => onReasoningEffortChange(event.target.value as ReasoningEffort)}
					>
						{reasoningEfforts.map((effort) => (
							<option key={effort} value={effort}>{effort}</option>
						))}
					</select>
				</label>
				<ConnectionBlockedReason reason={controlsBlockedReason} className="header-blocked-reason" />
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

export function NoticeStack({
	notices,
	rightOpen,
	onDismiss,
}: {
	notices: Notice[];
	rightOpen: boolean;
	onDismiss?: (noticeId: string) => void;
}) {
	if (notices.length === 0) return null;
	return (
		<div className={`notice-stack ${rightOpen ? "with-inspector" : ""}`} aria-live="polite">
			{notices.slice(-4).map((notice) => (
				<div className={`notice-toast ${notice.tone}`} key={notice.id}>
					<span>{notice.text}</span>
					{notice.persistent && onDismiss ? (
						<button
							type="button"
							className="notice-dismiss"
							aria-label="Dismiss notification"
							onClick={() => onDismiss(notice.id)}
						>
							<X size={14} />
						</button>
					) : null}
				</div>
			))}
		</div>
	);
}

export function Inspector({
	snapshot,
	runBoardParentSessionId = snapshot?.session_id ?? null,
	delegations,
	subagentNames = EMPTY_SUBAGENT_NAMES,
	hasMoreDelegations = false,
	delegationsLoading,
	delegationsError,
	showAllDelegations = false,
	expandedDelegationsAvailable = false,
	onToggleShowAllDelegations = () => {},
	onRetryDelegations,
	delegationsRetrying = false,
	selectedSessionId = null,
	boundedExpansionHasMore = false,
	onCancelDelegation,
	mutationBlockedReason,
	remoteReadBlockedReason,
	tools,
	onSelectSession,
	onClose
}: {
	snapshot: SessionSnapshot | null;
	runBoardParentSessionId?: string | null;
	delegations: Delegation[];
	subagentNames?: ReadonlyMap<string, string>;
	hasMoreDelegations?: boolean;
	delegationsLoading: boolean;
	delegationsError: string | null;
	showAllDelegations?: boolean;
	expandedDelegationsAvailable?: boolean;
	onToggleShowAllDelegations?: () => void;
	onRetryDelegations?: () => void;
	delegationsRetrying?: boolean;
	selectedSessionId?: string | null;
	boundedExpansionHasMore?: boolean;
	onCancelDelegation: (parentSessionId: string, delegationId: string) => void | Promise<void>;
	mutationBlockedReason?: string | null;
	remoteReadBlockedReason?: string | null;
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
						parentSessionId={runBoardParentSessionId}
						delegations={delegations}
						subagentNames={subagentNames}
						hasMoreDelegations={hasMoreDelegations}
						loading={delegationsLoading}
						error={delegationsError}
						showAllDelegations={showAllDelegations}
						expandedDelegationsAvailable={expandedDelegationsAvailable}
						onToggleShowAllDelegations={onToggleShowAllDelegations}
						onRetryDelegations={onRetryDelegations}
						delegationsRetrying={delegationsRetrying}
						selectedSessionId={selectedSessionId}
						boundedExpansionHasMore={boundedExpansionHasMore}
						onSelectSession={onSelectSession}
						onCancelDelegation={onCancelDelegation}
						mutationBlockedReason={mutationBlockedReason}
						remoteReadBlockedReason={remoteReadBlockedReason}
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
