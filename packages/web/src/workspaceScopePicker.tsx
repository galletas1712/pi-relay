import { ChevronDown, ChevronRight, Folder, FolderGit2, FolderTree } from "lucide-react";
import { memo, useId, useState } from "react";
import type { WorkspaceScopeEntry } from "./workspaceScope.ts";

/** Selects the project workspaces and optional git branches for the next session. */
export const WorkspaceScopePicker = memo(function WorkspaceScopePicker({
	scope,
	onChange,
	disabled,
	open: controlledOpen,
	onOpenChange,
}: {
	scope: WorkspaceScopeEntry[];
	onChange: (scope: WorkspaceScopeEntry[]) => void;
	disabled?: boolean;
	open?: boolean;
	onOpenChange?: (open: boolean) => void;
}) {
	const idPrefix = useId();
	const [internalOpen, setInternalOpen] = useState(false);
	const open = controlledOpen ?? internalOpen;
	if (!scope.length) return null;

	const panelId = `${idPrefix}-workspaces-panel`;
	const minimumId = `${idPrefix}-workspace-minimum`;
	const includedCount = scope.filter((entry) => entry.included).length;
	const summary = workspaceSummary(includedCount, scope.length);
	const setOpen = (nextOpen: boolean) => {
		if (controlledOpen === undefined) setInternalOpen(nextOpen);
		onOpenChange?.(nextOpen);
	};
	const patch = (index: number, change: Partial<WorkspaceScopeEntry>) => {
		onChange(scope.map((entry, entryIndex) => (entryIndex === index ? { ...entry, ...change } : entry)));
	};

	return (
		<div className="workspace-scope">
			<button
				type="button"
				className="workspace-scope-toggle"
				onClick={() => setOpen(!open)}
				aria-expanded={open}
				aria-controls={open ? panelId : undefined}
				disabled={disabled}
			>
				<FolderTree className="setup-disclosure-icon" size={18} aria-hidden />
				<span className="setup-disclosure-title">Workspaces</span>
				<span className="setup-disclosure-summary">
					{summary}
				</span>
				{open
					? <ChevronDown className="setup-disclosure-chevron" size={16} aria-hidden />
					: <ChevronRight className="setup-disclosure-chevron" size={16} aria-hidden />}
			</button>
			<span className="sr-only" role="status" aria-live="polite" aria-atomic="true">
				Workspace selection: {summary}.
			</span>
			{open ? (
				<div className="workspace-scope-list" id={panelId}>
					<p className="workspace-scope-help" id={minimumId}>Minimum 1 workspace</p>
					{scope.map((entry, index) => (
						<div className="workspace-scope-item" key={entry.workspaceDir}>
							<label className="workspace-scope-name">
								<input
									type="checkbox"
									checked={entry.included}
									disabled={disabled || (entry.included && includedCount === 1)}
									title={
										entry.included && includedCount === 1
											? "Minimum 1 workspace"
											: undefined
									}
									aria-describedby={
										entry.included && includedCount === 1 ? minimumId : undefined
									}
									onChange={(event) => patch(index, { included: event.target.checked })}
								/>
								{entry.kind === "git"
									? <FolderGit2 size={14} aria-hidden />
									: <Folder size={14} aria-hidden />}
								<span>{entry.workspaceDir}</span>
							</label>
							<div className="workspace-scope-detail">
								{entry.kind === "git" ? (
									<input
										className="workspace-scope-branch"
										value={entry.branch}
										placeholder="default branch"
										disabled={disabled || !entry.included}
										onChange={(event) => patch(index, { branch: event.target.value })}
										aria-label={`branch for ${entry.workspaceDir}`}
									/>
								) : null}
							</div>
						</div>
					))}
				</div>
			) : null}
		</div>
	);
});

function workspaceSummary(included: number, total: number): string {
	if (included === total) {
		return total === 1 ? "1 workspace included" : `All ${total} workspaces included`;
	}
	return `${included} of ${total} workspaces included`;
}
