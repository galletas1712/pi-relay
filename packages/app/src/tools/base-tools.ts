import {
	createApplyPatchToolDefinition,
	createBashToolDefinition,
	createEditToolDefinition,
	createFileAccessTracker,
	createReadToolDefinition,
	createWriteToolDefinition,
	type AgentSessionServices,
	type ToolDefinition,
} from "@mariozechner/pi-coding-agent";

export const RELAY_BASE_TOOL_NAMES = ["read", "bash", "edit", "apply_patch", "write"] as const;

export type RelayBaseToolDefinitionsFactory = () => ToolDefinition<any, any, any>[];

export function createRelayBaseToolDefinitionsFactory(
	cwd: string,
	settingsManager: AgentSessionServices["settingsManager"],
): RelayBaseToolDefinitionsFactory {
	const tracker = createFileAccessTracker();

	return () => {
		const autoResizeImages = settingsManager.getImageAutoResize();
		const shellCommandPrefix = settingsManager.getShellCommandPrefix();
		return [
			createReadToolDefinition(cwd, { autoResizeImages, tracker }),
			createBashToolDefinition(cwd, { commandPrefix: shellCommandPrefix }),
			createEditToolDefinition(cwd, { tracker }),
			createApplyPatchToolDefinition(cwd, { tracker }),
			createWriteToolDefinition(cwd, { tracker }),
		];
	};
}
