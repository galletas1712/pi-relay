import { X } from "lucide-react";
import { useState } from "react";
import { RunBoard } from "./runBoard.tsx";
import { COMMANDS } from "./slash.ts";
import type { Delegation, SessionSnapshot, ToolListing } from "./types.ts";

const EMPTY_SUBAGENT_NAMES = new Map<string, string>();

type InspectorTab = "run-board" | "debug";

const INSPECTOR_TABS: { id: InspectorTab; label: string }[] = [
	{ id: "run-board", label: "Agents" },
	{ id: "debug", label: "Inspector" },
];

function pendingActionLabel(action: SessionSnapshot["pending_actions"][number]): string {
	if (action.kind !== "compaction") return action.kind;
	return action.payload.trigger === "auto" ? "auto-compaction" : "compaction";
}

export interface InspectorProps {
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
}: InspectorProps) {
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
