import {
	Ban,
	CircleCheck,
	CircleAlert,
	CircleDashed,
	CircleHelp,
	CircleX,
	Clock3,
	Loader2,
	Square,
	TriangleAlert,
} from "lucide-react";
import { useMemo, useRef, useState } from "react";
import { ConnectionBlockedReason } from "./connectionRecovery.tsx";
import {
	agentStatusIconKey,
	isDelegationRunning,
	orderDelegations,
	statusIconClass,
	type AgentStatusIconKey,
} from "./delegationBoard.ts";
import type { Delegation, DelegationSubagent } from "./types.ts";

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
			<Icon className={iconKey === "running" ? "spin" : undefined} size={16} aria-hidden />
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
	onStop,
	mutationBlockedReason,
}: {
	delegation: Delegation;
	subagentNames: ReadonlyMap<string, string>;
	selectedSessionId?: string | null;
	actionState?: DelegationActionState;
	onStop: (delegation: Delegation) => void;
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
							className="stop-button run-board-stop"
							type="button"
							disabled={actionDisabled}
							aria-busy={pending}
							aria-label="stop delegated work"
							onClick={() => onStop(delegation)}
							title="stop delegated work"
						>
							{pending
								? <Loader2 className="spin" size={15} aria-hidden />
								: <Square size={14} aria-hidden />}
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
		if (actionLocks.current.has(key)) return;
		actionLocks.current.add(key);
		setActionState(key, { pending: true, error: null });
		try {
			await callback();
			setActionState(key, { pending: false, error: null });
		} catch (error) {
			setActionState(key, { pending: false, error: actionErrorMessage(error) });
		} finally {
			actionLocks.current.delete(key);
		}
	};
	const stopDelegation = (delegation: Delegation) => {
		if (mutationBlockedReason) return;
		void runAction(
			parentSessionId,
			delegation,
			() => onCancelDelegation(parentSessionId, delegation.delegation_id),
		);
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
								onStop={stopDelegation}
								mutationBlockedReason={mutationBlockedReason}
							/>
						))}
					</div>
					{showToggle ? (
						<>
							<button
								className="link-button run-board-toggle"
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
		</div>
	);
}

export function RunBoard({
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
			{parentSessionId && loading && delegations.length === 0 ? (
				<p className="muted run-board-inline-status" role="status">
					Loading agents…
				</p>
			) : null}
			{parentSessionId && error ? (
				<div className="load-error-banner run-board-load-error" role="alert">
					<div>
						<strong>Couldn’t load agents</strong>
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
