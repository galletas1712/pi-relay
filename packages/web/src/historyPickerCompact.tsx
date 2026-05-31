import { useMemo, useState, type CSSProperties, type ReactNode } from "react";
import { ChevronRight, Loader2, RotateCcw, X } from "lucide-react";
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
	onSwitch
}: {
	nodes: TranscriptTreeNode[];
	activeLeafId: string | null;
	loading?: boolean;
	error?: string | null;
	onClose: () => void;
	onSwitch: (target: HistoryTargetOption) => void;
}) {
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

	return (
		<div className="modal-scrim" role="presentation" onMouseDown={onClose}>
			<div
				className="history-dialog"
				role="dialog"
				aria-modal="true"
				aria-labelledby="history-dialog-title"
				onMouseDown={(event) => event.stopPropagation()}
			>
				<div className="history-dialog-head">
					<span className="history-dialog-icon">
						<RotateCcw size={15} />
					</span>
					<div className="history-dialog-copy">
						<h2 id="history-dialog-title">Switch branch</h2>
						<p>Pick a user message to edit, or a completed turn or compaction root to make active.</p>
					</div>
					<button className="plain-close-button" type="button" onClick={onClose} aria-label="close picker">
						<X size={14} />
					</button>
				</div>

				<div className="history-options tree" role="tree" aria-label="switch targets">
					{historyPickerContent({
						loading,
						error,
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
			</div>
		</div>
	);
}

function historyPickerContent({
	loading,
	error,
	renderedRows,
	hiddenBranchIds,
	onSwitch,
	onToggleBranch,
}: HistoryPickerContentParams): ReactNode {
	if (loading && renderedRows.length === 0) {
		return (
			<div className="history-loading">
				<Loader2 className="spin" size={16} />
				<span>Loading history index...</span>
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
								aria-label={isCollapsedBranch ? "expand branch" : "collapse branch"}
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
					<span>Loading more history...</span>
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
