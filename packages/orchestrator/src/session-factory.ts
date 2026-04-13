import {
	createAgentSessionFromServices,
	SessionManager,
	type AgentSessionServices,
	type ToolDefinition,
} from "@mariozechner/pi-coding-agent";
import type { AgentSessionFactory, AgentSessionFactoryOptions, AgentSessionHandle } from "./types.js";

function resolveBuiltInTools(parentSession: AgentSessionHandle, selectedToolNames: string[] | undefined): unknown[] | undefined {
	if (!selectedToolNames || selectedToolNames.length === 0) {
		return undefined;
	}

	const allowed = new Set(selectedToolNames);
	return parentSession.agent.state.tools
		.filter((tool) => allowed.has(tool.name))
		.map((tool) => tool as unknown);
}

export function createRelaySessionFactory(options: {
	services: AgentSessionServices;
	defaultSessionDir: string;
}): AgentSessionFactory {
	return async (sessionOptions: AgentSessionFactoryOptions) => {
		const sessionDir = sessionOptions.sessionDir ?? options.defaultSessionDir;
		const sessionManager =
			sessionOptions.mode === "restore" && sessionOptions.sessionFile
				? SessionManager.open(sessionOptions.sessionFile, sessionDir)
				: SessionManager.create(options.services.cwd, sessionDir);

		const created = await createAgentSessionFromServices({
			services: options.services,
			sessionManager,
			model: sessionOptions.config.model ?? sessionOptions.parentSession.model,
			thinkingLevel: sessionOptions.config.thinkingLevel ?? sessionOptions.parentSession.thinkingLevel,
			tools: resolveBuiltInTools(sessionOptions.parentSession, sessionOptions.config.tools) as never,
			customTools: sessionOptions.customTools as ToolDefinition[],
		});

		return {
			session: created.session,
		};
	};
}
