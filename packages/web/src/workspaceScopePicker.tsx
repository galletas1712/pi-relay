import { memo, useState } from "react";
import { ChevronDown, ChevronRight, FolderGit2, Folder } from "lucide-react";
import type { WorkspaceScopeEntry } from "./workspaceScope.ts";

/**
 * Inline picker shown above the composer when starting a new session in a project.
 *
 * It scopes the next session to a subset of the project's workspaces and lets git
 * workspaces start from a non-default branch, without blocking the type-and-send flow:
 * the control is collapsed by default and defaults to every workspace included.
 */
export const WorkspaceScopePicker = memo(function WorkspaceScopePicker({
	scope,
	onChange,
	disabled,
}: {
	scope: WorkspaceScopeEntry[];
	onChange: (scope: WorkspaceScopeEntry[]) => void;
	disabled?: boolean;
}) {
	const [open, setOpen] = useState(false);
	if (!scope.length) return null;

	const includedCount = scope.filter((entry) => entry.included).length;
	const patch = (index: number, change: Partial<WorkspaceScopeEntry>) => {
		onChange(scope.map((entry, entryIndex) => (entryIndex === index ? { ...entry, ...change } : entry)));
	};

	return (
		<div className="workspace-scope">
			<button
				type="button"
				className="workspace-scope-toggle"
				onClick={() => setOpen((value) => !value)}
				aria-expanded={open}
			>
				{open ? <ChevronDown size={14} /> : <ChevronRight size={14} />}
				<span>Workspaces</span>
				<span className="workspace-scope-count">
					{includedCount} of {scope.length}
				</span>
			</button>
			{open ? (
				<div className="workspace-scope-list">
					{scope.map((entry, index) => (
						<div className="workspace-scope-item" key={entry.workspaceDir}>
							<label className="workspace-scope-name">
								<input
									type="checkbox"
									checked={entry.included}
									disabled={disabled}
									onChange={(event) => patch(index, { included: event.target.checked })}
								/>
								{entry.kind === "git" ? <FolderGit2 size={14} /> : <Folder size={14} />}
								<span>{entry.workspaceDir}</span>
							</label>
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
					))}
				</div>
			) : null}
		</div>
	);
});
