import { useRef, type RefObject } from "react";
import { GitFork, Loader2, RotateCcw } from "lucide-react";
import {
	AppDialog,
	DialogCloseButton,
	DialogDescription,
	DialogTitle,
} from "./dialog.tsx";
import { ConnectionBlockedReason } from "./connectionRecovery.tsx";
import type { HistoryTargetsResult } from "./types.ts";

export interface HistoryTargetOption {
	actionLeafId: string | null;
	expectedActiveLeafId: string | null;
	expectedTranscriptRevision: number;
	sourceEntryId: string;
	restoreEntryId: string;
	turnLabel: string;
	preview: string;
	meta: string;
}

export function HistoryTargetPickerDialog({
	targets,
	mode,
	loading,
	submitting,
	error,
	hasMore,
	onLoadMore,
	onClose,
	onSelect,
	mutationBlockedReason,
	returnFocusFallbackRef,
}: {
	targets: HistoryTargetOption[];
	mode: "fork" | "switch";
	loading: boolean;
	submitting: boolean;
	error: string | null;
	hasMore: boolean;
	onLoadMore: () => void;
	onClose: () => void;
	onSelect: (target: HistoryTargetOption) => void;
	mutationBlockedReason?: string | null;
	returnFocusFallbackRef?: RefObject<HTMLElement | null>;
}) {
	const titleRef = useRef<HTMLHeadingElement>(null);
	const isFork = mode === "fork";
	const Icon = isFork ? GitFork : RotateCcw;
	return (
		<AppDialog
			className="history-dialog"
			busy={submitting}
			initialFocusRef={titleRef}
			returnFocusFallbackRef={returnFocusFallbackRef}
			onDismiss={onClose}
		>
			<div className="history-dialog-head">
				<span className="history-dialog-icon" aria-hidden="true"><Icon size={15} /></span>
				<div className="history-dialog-copy">
					<DialogTitle ref={titleRef} tabIndex={-1}>{isFork ? "Fork session" : "Switch branch"}</DialogTitle>
					<DialogDescription>Pick a historical user message to restore and edit.</DialogDescription>
				</div>
				<DialogCloseButton label="close picker" disabled={submitting} />
			</div>
			<div className="history-options">
				<ConnectionBlockedReason reason={mutationBlockedReason} className="history-blocked-reason" />
				{error ? <div className="history-empty error">{error}</div> : null}
				<ul className="history-target-list" aria-label={`${mode} targets`}>
					{targets.map((target) => (
						<li key={target.sourceEntryId}>
							<button
								className="history-option"
								type="button"
								disabled={submitting || !!mutationBlockedReason}
								aria-label={`${isFork ? "Fork from" : "Switch to"} User message: ${target.preview}`}
								onClick={() => onSelect(target)}
							>
								<span className="history-option-icon">{target.turnLabel}</span>
								<span className="history-option-main">
									<span className="history-option-title">User message</span>
									<span className="history-option-preview">{target.preview}</span>
								</span>
								<span className="history-option-meta">{target.meta}</span>
							</button>
						</li>
					))}
				</ul>
				{loading ? <div className="history-loading"><Loader2 className="spin" size={16} /> Loading history…</div> : null}
				{!loading && targets.length === 0 && !error ? <div className="history-empty">No editable messages yet.</div> : null}
				{hasMore && !loading ? <button type="button" disabled={submitting} onClick={onLoadMore}>Load older messages</button> : null}
			</div>
		</AppDialog>
	);
}

export function historyTargetOptions(page: HistoryTargetsResult): HistoryTargetOption[] {
	return page.targets.map((target) => ({
		actionLeafId: target.target_leaf_id,
		expectedActiveLeafId: page.active_leaf_id,
		expectedTranscriptRevision: page.transcript_revision,
		sourceEntryId: target.entry_id,
		restoreEntryId: target.entry_id,
		turnLabel: target.turn_id ? `t${target.turn_id}` : "turn",
		preview: target.preview,
		meta: `${target.is_on_active_branch ? "active branch" : "alternate history"} · ${new Date(target.timestamp_ms).toLocaleString()}`,
	}));
}
