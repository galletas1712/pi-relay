import {
	createAgentSessionFromServices,
	SessionManager,
	type AgentSessionServices,
	type ToolDefinition,
} from "@mariozechner/pi-coding-agent";
import type { AgentSessionFactory, AgentSessionFactoryOptions, AgentSessionHandle } from "./types.js";

function resolveBuiltInTools(parentSession: AgentSessionHandle, selectedToolNames: string[] | undefined): unknown[] | undefined {
	const activeBuiltInNames = new Set(
		parentSession
			.getAllTools()
			.filter((tool) => tool.sourceInfo.source === "builtin")
			.map((tool) => tool.name),
	);
	const inheritedBuiltIns = parentSession.agent.state.tools.filter((tool) => activeBuiltInNames.has(tool.name));
	if (!selectedToolNames || selectedToolNames.length === 0) {
		return inheritedBuiltIns.map((tool) => tool as unknown);
	}

	const allowed = new Set(selectedToolNames);
	return inheritedBuiltIns.filter((tool) => allowed.has(tool.name)).map((tool) => tool as unknown);
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
		if (sessionOptions.mode === "spawn") {
			sessionManager.ensurePersisted();
		}

		const created = await createAgentSessionFromServices({
			services: options.services,
			sessionManager,
			model: sessionOptions.config.model ?? sessionOptions.parentSession.model,
			thinkingLevel: sessionOptions.config.thinkingLevel ?? sessionOptions.parentSession.thinkingLevel,
			tools: resolveBuiltInTools(sessionOptions.parentSession, sessionOptions.config.tools) as never,
			customTools: sessionOptions.customTools as ToolDefinition[],
			sessionStartEvent:
				sessionOptions.mode === "restore"
					? { type: "session_start", reason: "resume", previousSessionFile: sessionOptions.sessionFile }
					: { type: "session_start", reason: "fork", previousSessionFile: sessionOptions.parentSession.sessionFile },
		});

		return {
			session: created.session,
		};
	};
}
