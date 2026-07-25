export type WorkspaceMaterializePhase =
	| "refreshing_base"
	| "copying"
	| "branch_override"
	| "done"
	| "error";

export interface WorkspaceMaterializeProgress {
	workspace_dir: string;
	phase: WorkspaceMaterializePhase;
	index: number;
	total: number;
}

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

export function isUncertainSessionStartError(error: unknown): boolean {
	const message = error instanceof Error ? error.message : String(error ?? "");
	return (
		message === "websocket request timed out" ||
		message === "websocket closed" ||
		message === "websocket reconnecting"
	);
}
