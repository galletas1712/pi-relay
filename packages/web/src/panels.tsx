import {
	Edit3,
	PanelRightClose,
	PanelRightOpen,
	Plus,
	RefreshCw,
	Search,
	Settings
} from "lucide-react";
import { COMMANDS } from "./slash.ts";
import { sessionTitle, type SessionListItem } from "./sessionList.ts";
import { truncate } from "./text.ts";
import type { Activity, DaemonConfig, Notice, SessionSnapshot, ToolDefinition } from "./types.ts";

export function SidebarHeader({
	counts,
	total,
	connection
}: {
	counts: Record<Activity, number>;
	total: number;
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
			</div>
		</div>
	);
}

export function SidebarToolbar({
	query,
	onQueryChange,
	onNew,
	onRefresh,
	loading
}: {
	query: string;
	onQueryChange: (query: string) => void;
	onNew: () => void;
	onRefresh: () => void;
	loading: boolean;
}) {
	return (
		<div className="sidebar-toolbar">
			<div className="toolbar-actions">
				<button className="primary-button" type="button" onClick={onNew}>
					<Plus size={14} />
					New session
				</button>
				<button className="icon-button" type="button" onClick={onRefresh} aria-label="refresh" title="refresh">
					<RefreshCw size={14} className={loading ? "spin" : ""} />
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
	onRename
}: {
	session: SessionListItem;
	selected: boolean;
	onSelect: () => void;
	onRename: () => void;
}) {
	return (
		<button className={`session-row ${selected ? "selected" : ""}`} type="button" onClick={onSelect}>
			<span className={`status-rail ${session.activity}`} />
			<span className="session-main">
				<span className="session-title">{sessionTitle(session)}</span>
				<span className="session-sub">
					{session.provider.kind} - {session.provider.model}
				</span>
			</span>
			<span className="session-leaf">
				{session.active_leaf_id ? session.active_leaf_id.slice(0, 6) : "root"}
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
		</button>
	);
}

export function LogHeader({
	session,
	snapshot,
	rightOpen,
	onToggleRight
}: {
	session: SessionListItem | null;
	snapshot: SessionSnapshot | null;
	rightOpen: boolean;
	onToggleRight: () => void;
}) {
	return (
		<div className="log-header">
			<span className="dot ok" />
			<span>agent-log</span>
			{session ? (
				<span className="log-session">
					{sessionTitle(session)} - {snapshot?.activity ?? session.activity}
				</span>
			) : (
				<span className="log-session">no session selected</span>
			)}
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
	tools: ToolDefinition[];
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
							<span>leaf</span>
							<strong>{snapshot.active_leaf_id?.slice(0, 12) ?? "root"}</strong>
						</div>
						<div className="kv">
							<span>provider</span>
							<strong>
								{snapshot.provider.kind} {snapshot.provider.model}
							</strong>
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
						<span key={tool.name}>{tool.name}</span>
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
