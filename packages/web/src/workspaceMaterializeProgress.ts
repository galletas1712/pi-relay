import type { WorkspaceMaterializePhase, WorkspaceMaterializeProgress } from "./types.ts";

export function formatWorkspacePreparationStatus(
	progress: WorkspaceMaterializeProgress | null,
): string {
	if (!progress || progress.total <= 0) {
		return "Preparing workspaces…";
	}
	const phase = phaseLabel(progress.phase);
	return `${phase} ${progress.workspace_dir} (${progress.index}/${progress.total})…`;
}

function phaseLabel(phase: WorkspaceMaterializePhase): string {
	switch (phase) {
		case "refreshing_base":
			return "Refreshing";
		case "copying":
			return "Copying";
		case "branch_override":
			return "Fetching branch for";
		case "done":
			return "Prepared";
		case "error":
			return "Failed preparing";
	}
}
