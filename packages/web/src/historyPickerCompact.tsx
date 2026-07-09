import { useLayoutEffect, useMemo, useRef, useState, type CSSProperties, type ReactNode, type RefObject } from "react";
import { ChevronRight, Loader2, RotateCcw } from "lucide-react";
import {
	AppDialog,
	DialogCloseButton,
	DialogDescription,
	DialogTitle,
} from "./dialog.tsx";
import { ConnectionBlockedReason } from "./connectionRecovery.tsx";
import { displayParentIdForNode } from "./displayParent.ts";
import {
	historySwitchOptionsFromNodes,
	nodeBranchIds,
	type HistoryTargetOption
} from "./historyTargets.ts";
import { perfEnabled, perfLog, perfNow } from "./perf.ts";
import type { TranscriptTreeNode } from "./types.ts";

interface VisibleHistoryNodeRow {
	node: TranscriptTreeNode;
	option: HistoryTargetOption;
	depth: number;
	isActive: boolean;
	isOnActivePath: boolean;
	parentId: string | null;
	isBranchRoot: boolean;
	descendantCount: number;
}

interface HistoryPickerContentParams {
	loading: boolean;
	error: string | null;
	mutationBlockedReason?: string | null;
	renderedRows: VisibleHistoryNodeRow[];
	hiddenBranchIds: Set<string>;
	onSwitch: (target: HistoryTargetOption) => void;
	onToggleBranch: (entryId: string) => void;
}

export function CompactHistoryPickerDialog({
	nodes,
	activeLeafId,
	loading = false,
	error = null,
	onClose,
	onSwitch,
	mutationBlockedReason,
	returnFocusFallbackRef,
}: {
	nodes: TranscriptTreeNode[];
	activeLeafId: string | null;
	loading?: boolean;
	error?: string | null;
	onClose: () => void;
	onSwitch: (target: HistoryTargetOption) => void;
	mutationBlockedReason?: string | null;
	returnFocusFallbackRef?: RefObject<HTMLElement | null>;
}) {
	const titleRef = useRef<HTMLHeadingElement>(null);
	const optionsRef = useRef<HTMLDivElement>(null);
	const initializedScrollRef = useRef(false);
	const [expandedBranches, setExpandedBranches] = useState<Set<string>>(() => new Set());
	const options = useMemo(
		() => historySwitchOptionsFromNodes(nodes, activeLeafId),
		[activeLeafId, nodes]
	);
	const visibleRows = useMemo(
		() => {
			const shouldLogPerf = perfEnabled();
			const startedAt = perfNow();
			const rows = historyPickerNodeRows(nodes, options, activeLeafId);
			if (shouldLogPerf) {
				perfLog("historyPickerNodeRows", {
					nodes: nodes.length,
					options: options.length,
					rows: rows.length,
					deriveMs: Math.round(perfNow() - startedAt)
				});
			}
			return rows;
		},
		[activeLeafId, nodes, options]
	);
	const hiddenBranchIds = useMemo(() => {
		const hidden = new Set<string>();
		for (const row of visibleRows) {
			if (!row.isBranchRoot || row.isOnActivePath || expandedBranches.has(row.node.id)) continue;
			hidden.add(row.node.id);
		}
		let changed = true;
		while (changed) {
			changed = false;
			for (const row of visibleRows) {
				if (hidden.has(row.node.id) || !row.parentId || !hidden.has(row.parentId)) continue;
				hidden.add(row.node.id);
				changed = true;
			}
		}
		return hidden;
	}, [expandedBranches, visibleRows]);
	const renderedRows = visibleRows.filter((row) => !hiddenBranchIds.has(row.node.id) || row.isBranchRoot);
	const toggleBranch = (entryId: string) => {
		setExpandedBranches((current) => {
			const next = new Set(current);
			if (next.has(entryId)) next.delete(entryId);
			else next.add(entryId);
			return next;
		});
	};
	const targetCount = visibleRows.length;
	useLayoutEffect(() => {
		if (initializedScrollRef.current || loading || error || renderedRows.length === 0) return;
		const list = optionsRef.current;
		if (!list) return;
		const active = list.querySelector<HTMLElement>('[role="treeitem"][aria-selected="true"]');
		if (active) scrollHistoryTargetIntoView(list, active);
		else if (activeLeafId === null || nodes.some((node) => node.id === activeLeafId)) {
			list.scrollTop = Math.max(0, list.scrollHeight - list.clientHeight);
		} else {
			return;
		}
		initializedScrollRef.current = true;
	}, [activeLeafId, error, loading, nodes, renderedRows]);

	return (
		<AppDialog
			className="history-dialog"
			initialFocusRef={titleRef}
			returnFocusFallbackRef={returnFocusFallbackRef}
			onDismiss={onClose}
		>
			<div className="history-dialog-head">
				<span className="history-dialog-icon" aria-hidden="true">
					<RotateCcw size={15} />
				</span>
				<div className="history-dialog-copy">
					<DialogTitle ref={titleRef} tabIndex={-1}>Switch branch</DialogTitle>
					<DialogDescription>
						Pick a user message to edit, or a completed turn or compaction root to make active.
					</DialogDescription>
				</div>
				<DialogCloseButton label="close picker" />
			</div>

			<div ref={optionsRef} className="history-options tree" role="tree" aria-label="switch targets">
				<ConnectionBlockedReason reason={mutationBlockedReason} className="history-blocked-reason" />
				{historyPickerContent({
					loading,
					error,
					mutationBlockedReason,
					renderedRows,
					hiddenBranchIds,
					onSwitch,
					onToggleBranch: toggleBranch,
				})}
				{!loading && !error && targetCount === 0 ? (
					<div className="history-empty">
						No editable messages, completed turns, or compaction roots yet.
					</div>
				) : null}
			</div>
		</AppDialog>
	);
}

export function scrollHistoryTargetIntoView(list: HTMLElement, target: HTMLElement): void {
	const listRect = list.getBoundingClientRect();
	const targetRect = target.getBoundingClientRect();
	if (targetRect.top < listRect.top) {
		list.scrollTop += targetRect.top - listRect.top;
	} else if (targetRect.bottom > listRect.bottom) {
		list.scrollTop += targetRect.bottom - listRect.bottom;
	}
}

function historyPickerContent({
	loading,
	error,
	mutationBlockedReason,
	renderedRows,
	hiddenBranchIds,
	onSwitch,
	onToggleBranch,
}: HistoryPickerContentParams): ReactNode {
	if (loading) {
		return (
			<div className="history-loading">
				<Loader2 className="spin" size={16} />
				<span>Loading history index…</span>
			</div>
		);
	}
	if (error) return <div className="history-empty error">{error}</div>;
	return (
		<>
			{renderedRows.map((row) => {
				const display = row.option;
				const isCollapsedBranch = hiddenBranchIds.has(row.node.id);
				const canCollapse = row.isBranchRoot && !row.isOnActivePath;
				const branchToggleLabel = `${isCollapsedBranch ? "Expand" : "Collapse"} branch for ${display.title}: ${display.preview}${isCollapsedBranch ? `, ${row.descendantCount} hidden descendant${row.descendantCount === 1 ? "" : "s"}` : ""}`;
				const outcomeClass = nonGracefulOutcomeClass(display.outcome);
				return (
					<div
						key={row.node.id}
						className={`history-tree-item ${row.isOnActivePath ? "on-active-path" : ""} ${isCollapsedBranch ? "collapsed" : ""} ${outcomeClass}`}
						style={{ "--tree-depth": row.depth } as CSSProperties}
					>
						{canCollapse ? (
							<button
								className="branch-toggle"
								type="button"
								onClick={() => onToggleBranch(row.node.id)}
								aria-label={branchToggleLabel}
								aria-expanded={!isCollapsedBranch}
							>
								<ChevronRight size={13} />
							</button>
						) : null}
						<button
							className="history-option tree-row"
							type="button"
							role="treeitem"
							aria-selected={row.isActive}
							disabled={!!mutationBlockedReason}
							onClick={() => onSwitch(row.option)}
						>
							<span className="tree-guides" aria-hidden="true" />
							<span className={`history-option-icon ${row.parentId ? "" : "root"}`}>
								{display.turnLabel}
							</span>
							<span className="history-option-main">
								<span className="history-option-title">
									{display.title}
									{row.isActive ? <span className="history-badge">current</span> : null}
									{isCollapsedBranch ? <span className="history-badge muted">{row.descendantCount} hidden</span> : null}
									{display.outcome && display.outcome !== "Graceful" ? (
										<span className="history-badge danger">{display.outcome.toLowerCase()}</span>
									) : null}
								</span>
								<span className="history-option-preview">{display.preview}</span>
							</span>
							<span className="history-option-meta">{display.meta}</span>
						</button>
					</div>
				);
			})}
			{loading ? (
				<div className="history-loading">
					<Loader2 className="spin" size={16} />
					<span>Loading more history…</span>
				</div>
			) : null}
		</>
	);
}

function historyPickerNodeRows(
	nodes: TranscriptTreeNode[],
	options: HistoryTargetOption[],
	activeLeafId: string | null
): VisibleHistoryNodeRow[] {
	const byId = new Map(nodes.map((node) => [node.id, node]));
	const order = new Map(nodes.map((node, index) => [node.id, index]));
	const optionById = new Map(options.flatMap((option) => (option.id ? [[option.id, option] as const] : [])));
	const visibleNodes = nodes.filter((node) => optionById.has(node.id));
	const visibleIds = new Set(visibleNodes.map((node) => node.id));
	const activePath = new Set(nodeBranchIds(nodes, activeLeafId));
	const visibleAncestorCache = new Map<string, string | null>();

	const nearestVisibleAncestor = (node: TranscriptTreeNode): string | null => {
		const cached = visibleAncestorCache.get(node.id);
		if (cached !== undefined) return cached;
		const parentId = displayParentIdForNode(node, byId);
		let ancestor: string | null = null;
		if (parentId) {
			ancestor = visibleIds.has(parentId) ? parentId : nearestVisibleAncestor(byId.get(parentId)!);
		}
		visibleAncestorCache.set(node.id, ancestor);
		return ancestor;
	};

	const children = new Map<string | null, TranscriptTreeNode[]>();
	for (const node of visibleNodes) {
		const parentId = nearestVisibleAncestor(node);
		const siblings = children.get(parentId) ?? [];
		siblings.push(node);
		children.set(parentId, siblings);
	}
	for (const siblings of children.values()) {
		siblings.sort((left, right) => (order.get(left.id) ?? 0) - (order.get(right.id) ?? 0));
	}

	const sizeCache = new Map<string, number>();
	const branchSize = (nodeId: string): number => {
		const cached = sizeCache.get(nodeId);
		if (cached !== undefined) return cached;
		const size = 1 + (children.get(nodeId) ?? []).reduce((sum, child) => sum + branchSize(child.id), 0);
		sizeCache.set(nodeId, size);
		return size;
	};

	const rows: VisibleHistoryNodeRow[] = [];
	const visit = (node: TranscriptTreeNode, depth: number, parentId: string | null, isBranchRoot: boolean) => {
		const option = optionById.get(node.id);
		if (!option) return;
		rows.push({
			node,
			option,
			depth,
			isActive: activeLeafId === node.id,
			isOnActivePath: activePath.has(node.id),
			parentId,
			isBranchRoot,
			descendantCount: branchSize(node.id) - 1
		});
		const nodeChildren = children.get(node.id) ?? [];
		const hasSplit = nodeChildren.length > 1;
		const activeChild = nodeChildren.find((child) => activePath.has(child.id));
		for (const child of nodeChildren) {
			const isAlternateBranch = hasSplit && child.id !== activeChild?.id;
			visit(child, depth + (isAlternateBranch ? 1 : 0), node.id, hasSplit);
		}
	};
	for (const root of children.get(null) ?? []) visit(root, 0, null, false);
	return rows;
}

function nonGracefulOutcomeClass(outcome: HistoryTargetOption["outcome"]): string {
	if (outcome === "Crashed") return "turn-crashed";
	if (outcome === "Interrupted") return "turn-interrupted";
	return "";
}
