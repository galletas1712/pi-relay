import {
	createAgentSessionFromServices,
	SessionManager,
	type AgentSessionServices,
	type ToolDefinition,
} from "@pi-relay/coding-agent";
import type { AgentSessionFactory, AgentSessionFactoryOptions, AgentSessionHandle } from "./types.js";

type BaseToolDefinitionsFactory = () => ToolDefinition[];

function resolveActiveBaseToolNames(
	parentSession: AgentSessionHandle,
	baseToolNames: string[],
	selectedToolNames: string[] | undefined,
): string[] {
	const allowedBaseToolNames = new Set(baseToolNames);
	const inheritedBaseTools = parentSession.agent.state.tools
		.map((tool) => tool.name)
		.filter((name) => allowedBaseToolNames.has(name));
	if (!selectedToolNames || selectedToolNames.length === 0) {
		return inheritedBaseTools;
	}

	const allowedSelectedNames = new Set(selectedToolNames);
	return inheritedBaseTools.filter((name) => allowedSelectedNames.has(name));
}

export function createRelaySessionFactory(options: {
	services: AgentSessionServices;
	defaultSessionDir: string;
	baseToolNames: string[];
	createSessionBaseToolDefinitionsFactory: () => BaseToolDefinitionsFactory;
}): AgentSessionFactory {
	return async (sessionOptions: AgentSessionFactoryOptions) => {
		const sessionDir = sessionOptions.sessionDir ?? options.defaultSessionDir;
		const sessionManager =
			sessionOptions.mode === "restore" && sessionOptions.sessionFile
				? SessionManager.open(sessionOptions.sessionFile, sessionDir)
				: SessionManager.create(options.services.cwd, sessionDir);
		const baseToolDefinitionsFactory = options.createSessionBaseToolDefinitionsFactory();

		const created = await createAgentSessionFromServices({
			services: options.services,
			sessionManager,
			model: sessionOptions.config.model ?? sessionOptions.parentSession.model,
			thinkingLevel: sessionOptions.config.thinkingLevel ?? sessionOptions.parentSession.thinkingLevel,
			toolNames: resolveActiveBaseToolNames(
				sessionOptions.parentSession,
				options.baseToolNames,
				sessionOptions.config.tools,
			),
			baseToolDefinitionsFactory,
			customTools: sessionOptions.customTools as ToolDefinition[],
			messageCacheHints: sessionOptions.spawnCacheHints,
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
