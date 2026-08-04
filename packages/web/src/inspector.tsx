import { X } from "lucide-react";
import { useEffect, useState } from "react";
import type { AgentApi } from "./agentApi.ts";
import {
	AppDialog,
	DialogBody,
	DialogCloseButton,
	DialogHeader,
	DialogHeading,
	DialogTitle,
} from "./dialog.tsx";
import { FilesTab } from "./filesTab.tsx";
import { RunBoard } from "./runBoard.tsx";
import { COMMANDS } from "./slash.ts";
import type { Delegation, SessionSnapshot, SessionWorkspace, ToolListing } from "./types.ts";

const EMPTY_SUBAGENT_NAMES = new Map<string, string>();

export type InspectorTab = "run-board" | "files";

const INSPECTOR_TABS: { id: InspectorTab; label: string }[] = [
	{ id: "run-board", label: "Agents" },
	{ id: "files", label: "Files" },
];

function pendingActionLabel(action: SessionSnapshot["pending_actions"][number]): string {
	if (action.kind !== "compaction") return action.kind;
	return action.payload.trigger === "auto" ? "auto-compaction" : "compaction";
}

export interface InspectorProps {
	api?: AgentApi | null;
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
	selectedFilePath?: string | null;
	preferredTab?: InspectorTab | null;
	filesTreeEpoch?: number;
	onSelectFile?: (path: string) => void;
	onVisibleDirectoriesChange?: (directories: string[]) => void;
	onActiveTabChange?: (tab: InspectorTab) => void;
	onSelectSession?: (sessionId: string) => void;
	onClose?: () => void;
}

export function Inspector({
	api = null,
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
	selectedFilePath = null,
	preferredTab = null,
	filesTreeEpoch = 0,
	onSelectFile,
	onVisibleDirectoriesChange,
	onActiveTabChange,
	onSelectSession,
	onClose,
}: InspectorProps) {
	const [activeTab, setActiveTab] = useState<InspectorTab>(preferredTab ?? "run-board");
	const [inspectorDialogOpen, setInspectorDialogOpen] = useState(false);

	useEffect(() => {
		if (preferredTab) setActiveTab(preferredTab);
	}, [preferredTab]);

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
						onClick={() => {
							setActiveTab(tab.id);
							onActiveTabChange?.(tab.id);
						}}
					>
						{tab.label}
					</button>
				))}
				<button className="plain-close-button inspector-close" type="button" onClick={onClose} aria-label="close inspector">
					<X size={14} />
				</button>
			</div>
			<div
				className="inspector-tab-panel"
				role="tabpanel"
				id="inspector-panel-run-board"
				aria-labelledby="inspector-tab-run-board"
				hidden={activeTab !== "run-board"}
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
			<div
				className="inspector-tab-panel"
				role="tabpanel"
				id="inspector-panel-files"
				aria-labelledby="inspector-tab-files"
				hidden={activeTab !== "files"}
			>
				{api ? (
					<FilesTab
						key={selectedSessionId ?? "none"}
						api={api}
						sessionId={selectedSessionId}
						selectedPath={selectedFilePath}
						remoteReadBlockedReason={remoteReadBlockedReason}
						activity={snapshot?.activity ?? null}
						treeEpoch={filesTreeEpoch}
						onSelectFile={(path) => onSelectFile?.(path)}
						onVisibleDirectoriesChange={onVisibleDirectoriesChange}
					/>
				) : (
					<p className="muted">Files browser unavailable.</p>
				)}
			</div>
			<div className="inspector-footer">
				<button
					className="secondary-button inspector-show-details"
					type="button"
					onClick={() => setInspectorDialogOpen(true)}
				>
					Show inspector
				</button>
			</div>
			{inspectorDialogOpen ? (
				<AppDialog
					className="inspector-details-dialog"
					onDismiss={() => setInspectorDialogOpen(false)}
				>
					<DialogHeader>
						<DialogHeading>
							<DialogTitle>Inspector</DialogTitle>
						</DialogHeading>
						<DialogCloseButton label="close inspector" />
					</DialogHeader>
					<DialogBody>
						<InspectorDetails
							snapshot={snapshot}
							tools={tools}
							onSelectSession={onSelectSession}
						/>
					</DialogBody>
				</AppDialog>
			) : null}
		</div>
	);
}

function InspectorDetails({
	snapshot,
	tools,
	onSelectSession,
}: {
	snapshot: SessionSnapshot | null;
	tools: ToolListing[];
	onSelectSession?: (sessionId: string) => void;
}) {
	return (
		<>
			<section className="inspect-section">
				<h2>Workspace</h2>
				{snapshot ? (
					<>
						<div className="kv">
							<span>session cwd</span>
							<code title={snapshot.workspace_id}>{snapshot.workspace_id}</code>
						</div>
						<div className="kv">
							<span>runtime</span>
							<strong>{snapshot.runtime_id}</strong>
						</div>
						{snapshot.workspaces.length ? (
							<div className="inspect-workspace-list">
								{snapshot.workspaces.map((workspace) => (
									<WorkspaceContextRow key={workspace.workspace_dir} workspace={workspace} />
								))}
							</div>
						) : (
							<p className="muted">No materialized workspaces on this session.</p>
						)}
					</>
				) : (
					<p className="muted">No session loaded.</p>
				)}
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
						<span key={`${tool.kind}:${tool.name}`} title={tool.description || tool.name}>
							{tool.name}
						</span>
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
		</>
	);
}

function WorkspaceContextRow({ workspace }: { workspace: SessionWorkspace }) {
	const kind = workspace.kind ?? "git";
	return (
		<div className="inspect-workspace-row">
			<div className="inspect-workspace-row-head">
				<strong>{workspace.workspace_dir}</strong>
				<span className="inspect-workspace-kind">{kind}</span>
			</div>
			{workspace.local_branch ? (
				<div className="inspect-workspace-detail">
					<span>branch</span>
					<code>{workspace.local_branch}</code>
				</div>
			) : null}
			{workspace.remote_url ? (
				<div className="inspect-workspace-detail">
					<span>remote</span>
					<code>{workspace.remote_url}</code>
				</div>
			) : null}
			{workspace.source_path ? (
				<div className="inspect-workspace-detail">
					<span>source</span>
					<code>{workspace.source_path}</code>
				</div>
			) : null}
		</div>
	);
}
