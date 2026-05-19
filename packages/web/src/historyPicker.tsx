import { useMemo, useState, type CSSProperties } from "react";
import { ChevronRight, GitFork, Loader2, RotateCcw, X } from "lucide-react";
import {
	branchEntriesFor,
	historyForkOptions,
	historySwitchOptions,
	type HistoryTargetOption
} from "./historyTargets.ts";
import type { TranscriptEntry } from "./types.ts";

interface VisibleHistoryRow {
	entry: TranscriptEntry;
	option: HistoryTargetOption;
	depth: number;
	isActive: boolean;
	isOnActivePath: boolean;
	parentId: string | null;
	isBranchRoot: boolean;
	descendantCount: number;
}

export function HistoryPickerDialog({
	mode,
	entries,
	activeLeafId,
	initialForkTitle = "",
	loading = false,
	error = null,
	onClose,
	onFork,
	onSwitch
}: {
	mode: "fork" | "switch";
	entries: TranscriptEntry[];
	activeLeafId: string | null;
	initialForkTitle?: string;
	loading?: boolean;
	error?: string | null;
	onClose: () => void;
	onFork: (target: HistoryTargetOption, title: string) => void;
	onSwitch: (target: HistoryTargetOption) => void;
}) {
	const [forkTitle, setForkTitle] = useState(initialForkTitle);
	const [expandedBranches, setExpandedBranches] = useState<Set<string>>(() => new Set());
	const options = useMemo(
		() => {
			if (mode === "fork") return historyForkOptions(entries, activeLeafId);
			return historySwitchOptions(entries, activeLeafId);
		},
		[activeLeafId, entries, mode]
	);
	const visibleRows = useMemo(
		() => historyPickerRows(entries, options, activeLeafId),
		[activeLeafId, entries, options]
	);
	const hiddenBranchIds = useMemo(() => {
		const hidden = new Set<string>();
		for (const row of visibleRows) {
			if (!row.isBranchRoot || row.isOnActivePath || expandedBranches.has(row.entry.id)) continue;
			hidden.add(row.entry.id);
		}
		let changed = true;
		while (changed) {
			changed = false;
			for (const row of visibleRows) {
				if (hidden.has(row.entry.id) || !row.parentId || !hidden.has(row.parentId)) continue;
				hidden.add(row.entry.id);
				changed = true;
			}
		}
		return hidden;
	}, [expandedBranches, visibleRows]);
	const renderedRows = visibleRows.filter((row) => !hiddenBranchIds.has(row.entry.id) || row.isBranchRoot);
	const toggleBranch = (entryId: string) => {
		setExpandedBranches((current) => {
			const next = new Set(current);
			if (next.has(entryId)) next.delete(entryId);
			else next.add(entryId);
			return next;
		});
	};
	const targetCount = visibleRows.length;
	const title = mode === "fork" ? "Fork session" : "Switch branch";
	const description =
		mode === "fork"
			? "Pick a user message, completed turn, or compaction root to branch from."
			: "Pick a user message to edit, or a completed turn or compaction root to make active.";
	const Icon = mode === "fork" ? GitFork : RotateCcw;

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
						<Icon size={15} />
					</span>
					<div className="history-dialog-copy">
						<h2 id="history-dialog-title">{title}</h2>
						<p>{description}</p>
					</div>
					<button className="plain-close-button" type="button" onClick={onClose} aria-label="close picker">
						<X size={14} />
					</button>
				</div>

				{mode === "fork" ? (
					<label className="history-title-field">
						<span>Fork title</span>
						<input
							value={forkTitle}
							onChange={(event) => setForkTitle(event.target.value)}
							placeholder="Optional title"
							autoFocus
						/>
					</label>
				) : null}

				<div className="history-options tree" role="tree" aria-label={`${mode} targets`}>
					{loading ? (
						<div className="history-loading">
							<Loader2 className="spin" size={16} />
							<span>Loading full history...</span>
						</div>
					) : error ? (
						<div className="history-empty error">{error}</div>
					) : (
						renderedRows.map((row) => {
							const display = row.option;
							const isCollapsedBranch = hiddenBranchIds.has(row.entry.id);
							const canCollapse = row.isBranchRoot && !row.isOnActivePath;
							const outcomeClass = nonGracefulOutcomeClass(display.outcome);
							return (
								<div
									key={row.entry.id}
									className={`history-tree-item ${row.isOnActivePath ? "on-active-path" : ""} ${isCollapsedBranch ? "collapsed" : ""} ${outcomeClass}`}
									style={{ "--tree-depth": row.depth } as CSSProperties}
								>
									{canCollapse ? (
										<button
											className="branch-toggle"
											type="button"
											onClick={() => toggleBranch(row.entry.id)}
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
										onClick={() => {
											if (mode === "fork") {
												onFork(row.option, forkTitle);
											} else {
												onSwitch(row.option);
											}
										}}
									>
										<span className="tree-guides" aria-hidden="true" />
										<span className={`history-option-icon ${row.entry.parent_id ? "" : "root"}`}>
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
						})
					)}
					{!loading && !error && targetCount === 0 ? (
						<div className="history-empty">
							{mode === "fork"
								? "No user messages, completed turns, or compaction roots yet."
								: "No editable messages, completed turns, or compaction roots yet."}
						</div>
					) : null}
				</div>
			</div>
		</div>
	);
}

function historyPickerRows(
	entries: TranscriptEntry[],
	options: HistoryTargetOption[],
	activeLeafId: string | null
): VisibleHistoryRow[] {
	const byId = new Map(entries.map((entry) => [entry.id, entry]));
	const order = new Map(entries.map((entry, index) => [entry.id, index]));
	const optionById = new Map(options.flatMap((option) => (option.id ? [[option.id, option] as const] : [])));
	const visibleEntries = entries.filter((entry) => optionById.has(entry.id));
	const visibleIds = new Set(visibleEntries.map((entry) => entry.id));
	const activePath = new Set(branchEntriesFor(entries, activeLeafId).map((entry) => entry.id));

	const nearestVisibleAncestor = (entry: TranscriptEntry): string | null => {
		let cursor = entry.parent_id;
		const seen = new Set<string>();
		while (cursor && !seen.has(cursor)) {
			seen.add(cursor);
			if (visibleIds.has(cursor)) return cursor;
			cursor = byId.get(cursor)?.parent_id ?? null;
		}
		return null;
	};

	const children = new Map<string | null, TranscriptEntry[]>();
	for (const entry of visibleEntries) {
		const parentId = nearestVisibleAncestor(entry);
		const siblings = children.get(parentId) ?? [];
		siblings.push(entry);
		children.set(parentId, siblings);
	}
	for (const siblings of children.values()) {
		siblings.sort((left, right) => (order.get(left.id) ?? 0) - (order.get(right.id) ?? 0));
	}

	const sizeCache = new Map<string, number>();
	const branchSize = (entryId: string): number => {
		const cached = sizeCache.get(entryId);
		if (cached !== undefined) return cached;
		const size = 1 + (children.get(entryId) ?? []).reduce((sum, child) => sum + branchSize(child.id), 0);
		sizeCache.set(entryId, size);
		return size;
	};

	const rows: VisibleHistoryRow[] = [];
	const visit = (entry: TranscriptEntry, depth: number, parentId: string | null, isBranchRoot: boolean) => {
		const option = optionById.get(entry.id);
		if (!option) return;
		rows.push({
			entry,
			option,
			depth,
			isActive: activeLeafId === entry.id,
			isOnActivePath: activePath.has(entry.id),
			parentId,
			isBranchRoot,
			descendantCount: branchSize(entry.id) - 1
		});
		const entryChildren = children.get(entry.id) ?? [];
		const hasSplit = entryChildren.length > 1;
		const activeChild = entryChildren.find((child) => activePath.has(child.id));
		for (const child of entryChildren) {
			const isAlternateBranch = hasSplit && child.id !== activeChild?.id;
			visit(child, depth + (isAlternateBranch ? 1 : 0), entry.id, hasSplit);
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
