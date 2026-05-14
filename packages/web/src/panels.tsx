import {
	Archive,
	ArchiveRestore,
	Edit3,
	PanelRightClose,
	PanelRightOpen,
	Plus,
	Search,
	Settings,
	Trash2
} from "lucide-react";
import type { ModelOption } from "./sessionDefaults.ts";
import { COMMANDS } from "./slash.ts";
import { isArchivedSession, sessionTitle, type SessionListItem } from "./sessionList.ts";
import { truncate } from "./text.ts";
import type { Activity, DaemonConfig, Notice, ReasoningEffort, SessionSnapshot, ToolListing } from "./types.ts";

export function SidebarHeader({
	counts,
	total,
	archived,
	connection
}: {
	counts: Record<Activity, number>;
	total: number;
	archived: number;
	connection: string;
}) {
	return (
		<div className="sidebar-header">
			<div className="masthead">
				<span className={`dot ${connection === "open" ? "ok" : "warn"}`} />
				<span className="masthead-title">sessions</span>
				<span className="masthead-count">{total}</span>
			</div>
			<div className="activity-counts">
				{(["running", "queued", "idle"] as Activity[]).map((activity) => (
					<span className="activity-chip" key={activity}>
						<span className={`dot ${activity}`} />
						{activity}
						<span className="count">{counts[activity] ?? 0}</span>
					</span>
				))}
				<span className="activity-chip">
					<span className="dot archived" />
					archived
					<span className="count">{archived}</span>
				</span>
			</div>
		</div>
	);
}

export function SidebarToolbar({
	query,
	onQueryChange,
	showArchived,
	onToggleArchived,
	onNew
}: {
	query: string;
	onQueryChange: (query: string) => void;
	showArchived: boolean;
	onToggleArchived: () => void;
	onNew: () => void;
}) {
	return (
		<div className="sidebar-toolbar">
			<div className="toolbar-actions">
				<button className="primary-button" type="button" onClick={onNew}>
					<Plus size={14} />
					New session
				</button>
				<button
					className={`icon-button ${showArchived ? "pressed" : ""}`}
					type="button"
					onClick={onToggleArchived}
					aria-label={showArchived ? "hide archived sessions" : "show archived sessions"}
					title={showArchived ? "hide archived sessions" : "show archived sessions"}
				>
					<Archive size={14} />
				</button>
			</div>
			<label className="search-box">
				<Search size={14} />
				<input value={query} onChange={(event) => onQueryChange(event.target.value)} placeholder="filter sessions..." />
			</label>
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
	const canArchive = session.activity === "idle";
	const canDelete = session.activity === "idle";
	const ArchiveIcon = archived ? ArchiveRestore : Archive;
	return (
		<button className={`session-row ${selected ? "selected" : ""} ${archived ? "archived" : ""}`} type="button" onClick={onSelect}>
			<span className={`status-rail ${archived ? "archived" : session.activity}`} />
			<span className="session-main">
				<span className="session-title">{sessionTitle(session)}</span>
				<span className="session-sub">
					{archived ? "archived - " : ""}{session.provider.kind} - {session.provider.model}
				</span>
				<span className="session-leaf">
					{session.active_leaf_id ? session.active_leaf_id.slice(0, 6) : "root"}
				</span>
			</span>
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
		</button>
	);
}

export function LogHeader({
	session,
	snapshot,
	modelOptions,
	modelValue,
	modelLocked,
	modelControlsDisabled,
	reasoningEfforts,
	reasoningEffort,
	onModelChange,
	onReasoningEffortChange,
	rightOpen,
	onToggleRight
}: {
	session: SessionListItem | null;
	snapshot: SessionSnapshot | null;
	modelOptions: ModelOption[];
	modelValue: string;
	modelLocked: boolean;
	modelControlsDisabled: boolean;
	reasoningEfforts: ReasoningEffort[];
	reasoningEffort: ReasoningEffort;
	onModelChange: (value: string) => void;
	onReasoningEffortChange: (value: ReasoningEffort) => void;
	rightOpen: boolean;
	onToggleRight: () => void;
}) {
	const archived = session ? isArchivedSession(session) : false;
	const modelDisabled = modelLocked || modelControlsDisabled;
	const displayedModelOptions = modelOptions.some((option) => option.id === modelValue)
		? modelOptions
		: [{ id: modelValue, label: modelValue }, ...modelOptions];
	const displayedEfforts = reasoningEfforts.includes(reasoningEffort)
		? reasoningEfforts
		: [reasoningEffort, ...reasoningEfforts];
	return (
		<div className="log-header">
			<span className={`dot ${archived ? "archived" : "ok"}`} />
			<span>agent-log</span>
			{session ? (
				<span className="log-session">
					{sessionTitle(session)} - {archived ? "archived" : (snapshot?.activity ?? session.activity)}
				</span>
			) : (
				<span className="log-session">no session selected</span>
			)}
			<div className="log-controls">
				<label className="header-select">
					<span>model</span>
					<select
						value={modelValue}
						disabled={modelDisabled}
						title={modelLocked ? "model is locked after the first transcript entry" : "model"}
						onChange={(event) => onModelChange(event.target.value)}
					>
						{displayedModelOptions.map((option) => (
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
						{displayedEfforts.map((effort) => (
							<option key={effort} value={effort}>{effort}</option>
						))}
					</select>
				</label>
			</div>
			<button className="icon-button tiny" type="button" onClick={onToggleRight} title={rightOpen ? "close inspector" : "open inspector"}>
				{rightOpen ? <PanelRightClose size={14} /> : <PanelRightOpen size={14} />}
			</button>
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
	config,
	tools
}: {
	snapshot: SessionSnapshot | null;
	config: DaemonConfig;
	tools: ToolListing[];
}) {
	return (
		<div className="inspector-inner">
			<div className="inspector-head">
				<Settings size={14} />
				<span>inspector</span>
			</div>
			<section className="inspect-section">
				<h2>Global</h2>
				<div className="kv">
					<span>system</span>
					<strong>{config.system_prompt ? truncate(config.system_prompt, 80) : "empty"}</strong>
				</div>
			</section>
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
			<section className="inspect-section">
				<h2>Pending</h2>
				{snapshot?.pending_actions.length ? (
					<div className="pending-list">
						{snapshot.pending_actions.map((action) => (
							<div className="pending-row" key={action.action_row_id}>
								<span>{action.kind}</span>
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
						<span key={tool.name} title={tool.name}>{tool.pretty_name}</span>
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
