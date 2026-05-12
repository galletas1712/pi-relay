import { useMemo, useState, type CSSProperties } from "react";
import { GitFork, RotateCcw, X } from "lucide-react";
import {
	historyEntryDisplay,
	historyForkOptions,
	historySwitchOptions,
	historyTreeRows,
	type HistoryTargetOption
} from "./historyTargets.ts";
import type { TranscriptEntry } from "./types.ts";

export function HistoryPickerDialog({
	mode,
	entries,
	activeLeafId,
	initialForkTitle = "",
	onClose,
	onFork,
	onSwitch
}: {
	mode: "fork" | "switch";
	entries: TranscriptEntry[];
	activeLeafId: string | null;
	initialForkTitle?: string;
	onClose: () => void;
	onFork: (target: HistoryTargetOption, title: string) => void;
	onSwitch: (target: HistoryTargetOption) => void;
}) {
	const [forkTitle, setForkTitle] = useState(initialForkTitle);
	const options = useMemo(
		() => {
			if (mode === "fork") return historyForkOptions(entries, activeLeafId);
			return historySwitchOptions(entries, activeLeafId);
		},
		[activeLeafId, entries, mode]
	);
	const optionsByEntryId = useMemo(
		() => new Map(options.flatMap((option) => (option.id ? [[option.id, option] as const] : []))),
		[options]
	);
	const rows = useMemo(() => historyTreeRows(entries, activeLeafId), [activeLeafId, entries]);
	const targetCount = options.filter((option) => option.id).length;
	const title = mode === "fork" ? "Fork session" : "Switch branch";
	const description =
		mode === "fork"
			? "Pick the transcript point the new session should branch from."
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
					<button className="icon-button tiny" type="button" onClick={onClose} aria-label="close picker">
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
					{rows.map((row) => {
						const option = optionsByEntryId.get(row.entry.id);
						const display = option ?? historyEntryDisplay(row.entry, entries);
						const disabled = !option;
						return (
							<button
								key={row.entry.id}
								className={`history-option tree-row ${row.isOnActivePath ? "on-active-path" : ""}`}
								style={{ "--tree-depth": row.depth } as CSSProperties}
								type="button"
								disabled={disabled}
								role="treeitem"
								aria-selected={row.isActive}
								aria-disabled={disabled}
								onClick={() => {
									if (!option) return;
									if (mode === "fork") {
										onFork(option, forkTitle);
									} else {
										onSwitch(option);
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
										{disabled ? <span className="history-badge muted">view</span> : null}
									</span>
									<span className="history-option-preview">{display.preview}</span>
								</span>
								<span className="history-option-meta">{display.meta}</span>
							</button>
						);
					})}
					{targetCount === 0 ? (
						<div className="history-empty">
							{mode === "fork"
								? "No transcript entries yet."
								: "No editable messages, completed turns, or compaction roots yet."}
						</div>
					) : null}
				</div>
			</div>
		</div>
	);
}
