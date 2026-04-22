import { getModel } from "@pi-relay/ai";
import {
	type CreateAgentSessionRuntimeFactory,
	createAgentSessionFromServices,
	createAgentSessionRuntime,
	createAgentSessionServices,
	getAgentDir,
	SessionManager,
	type SettingsManager as RelaySettingsManager,
	type ToolDefinition,
} from "@pi-relay/coding-agent";
import {
	type AgentSessionHandle,
	createMessageTool,
	createOrchestratorExtension,
	Orchestrator,
	createRelaySessionFactory,
	createSpawnTool,
} from "@pi-relay/orchestrator";
import { RelayRuntimeHost, type RelayRuntimeStateRef } from "./relay-runtime-host.js";
import { createRelayBaseToolDefinitionsFactory, RELAY_BASE_TOOL_NAMES } from "./tools/base-tools.js";

const RELAY_APPEND_SYSTEM_PROMPT = `Relay tool usage:
- Use read instead of cat, head, tail, or sed for reading files.
- Use apply_patch for multi-file or diff-shaped changes to existing files.
- Use edit for precise replacements inside one existing file.
- Use write only for new files or complete rewrites.
- Do not use bash to read or edit files when dedicated tools are available.
- After apply_patch succeeds, do not immediately re-read the same file unless you need verification or nearby context.`;

export const RELAY_RUNTIME_ENGINE_MODES = ["legacy", "ts-core", "rust-shadow", "rust"] as const;

export type RelayRuntimeEngineMode = (typeof RELAY_RUNTIME_ENGINE_MODES)[number];

export interface RelayRuntimeEngineConfig {
	orchestrator: RelayRuntimeEngineMode;
	session: RelayRuntimeEngineMode;
}

export const DEFAULT_RELAY_RUNTIME_ENGINE_CONFIG: Readonly<RelayRuntimeEngineConfig> = {
	orchestrator: "legacy",
	session: "legacy",
};

function parseRelayRuntimeEngineMode(
	envName: "PI_RELAY_ORCH_ENGINE" | "PI_RELAY_SESSION_ENGINE",
	value: string | undefined,
	fallback: RelayRuntimeEngineMode,
): RelayRuntimeEngineMode {
	if (!value) {
		return fallback;
	}

	if ((RELAY_RUNTIME_ENGINE_MODES as readonly string[]).includes(value)) {
		return value as RelayRuntimeEngineMode;
	}

	throw new Error(
		`Invalid ${envName}=${JSON.stringify(value)}. Expected one of: ${RELAY_RUNTIME_ENGINE_MODES.join(", ")}.`,
	);
}

export function resolveRelayRuntimeEngineConfig(env: NodeJS.ProcessEnv = process.env): RelayRuntimeEngineConfig {
	return {
		orchestrator: parseRelayRuntimeEngineMode(
			"PI_RELAY_ORCH_ENGINE",
			env.PI_RELAY_ORCH_ENGINE,
			DEFAULT_RELAY_RUNTIME_ENGINE_CONFIG.orchestrator,
		),
		session: parseRelayRuntimeEngineMode(
			"PI_RELAY_SESSION_ENGINE",
			env.PI_RELAY_SESSION_ENGINE,
			DEFAULT_RELAY_RUNTIME_ENGINE_CONFIG.session,
		),
	};
}

export function parseArgs(argv: string[]) {
	const args = [...argv];
	let mode: "interactive" | "rpc" = "interactive";
	let initialMessage: string | undefined;

	while (args.length > 0) {
		const arg = args.shift();
		if (arg === "--rpc") {
			mode = "rpc";
			continue;
		}
		if (!initialMessage) {
			initialMessage = arg;
		}
	}

	return { mode, initialMessage };
}

export function createRelayRuntimeFactory(
	agentDir = getAgentDir(),
	stateRef: RelayRuntimeStateRef = {},
): CreateAgentSessionRuntimeFactory {
	const engineConfig = resolveRelayRuntimeEngineConfig();
	const orchestratorUiRef: { cleanup?: () => void; sessionId?: string } = {};
	return async ({ cwd, sessionManager, sessionStartEvent }) => {
		const orchestratorRef: { current?: Orchestrator } = {};
		const settingsManagerRef: { current?: RelaySettingsManager } = {};
		const services = await createAgentSessionServices({
			cwd,
			agentDir,
			resourceLoaderOptions: {
				appendSystemPrompt: [RELAY_APPEND_SYSTEM_PROMPT],
				extensionFactories: [
					createOrchestratorExtension(orchestratorRef, orchestratorUiRef, {
						getSettingsManager: () => settingsManagerRef.current,
					}),
				],
			},
		});
		settingsManagerRef.current = services.settingsManager;
		const rootToolBridge = {
			async spawnAgent(parentId: string, config: Parameters<Orchestrator["spawnAgent"]>[1]) {
				if (!orchestratorRef.current) {
					throw new Error("Orchestrator has not been initialized yet");
				}
				return orchestratorRef.current.spawnAgent(parentId, config);
			},
			async routeMessage(fromAgentId: string, targetAgentId: string, content: string) {
				if (!orchestratorRef.current) {
					throw new Error("Orchestrator has not been initialized yet");
				}
				await orchestratorRef.current.routeMessage(fromAgentId, targetAgentId, content);
			},
		};
		const rootTools: ToolDefinition[] = [
			createSpawnTool(rootToolBridge, "root") as unknown as ToolDefinition,
			createMessageTool(rootToolBridge, "root") as unknown as ToolDefinition,
		];
		const createSessionBaseToolDefinitionsFactory = () =>
			createRelayBaseToolDefinitionsFactory(cwd, services.settingsManager);
		const rootBaseToolDefinitionsFactory = createSessionBaseToolDefinitionsFactory();
		const created = await createAgentSessionFromServices({
			services,
			sessionManager,
			sessionStartEvent,
			toolNames: [...RELAY_BASE_TOOL_NAMES],
			baseToolDefinitionsFactory: rootBaseToolDefinitionsFactory,
			customTools: rootTools,
		});
		const orchestrator = new Orchestrator({
			rootSession: created.session as unknown as AgentSessionHandle,
			sessionFactory: createRelaySessionFactory({
				services,
				defaultSessionDir: sessionManager.getSessionDir(),
				baseToolNames: [...RELAY_BASE_TOOL_NAMES],
				createSessionBaseToolDefinitionsFactory,
			}),
		});
		// Hydrate the worklog-fork model override from persisted settings so
		// a prior `/worklog-model` choice survives process restarts. If no
		// choice was saved, the fork falls back to the session's main model.
		// Guarded with `typeof ... === "function"` so stubbed settings
		// managers in tests (and older SettingsManager instances that might
		// not have been updated yet) don't crash startup.
		const sm = services.settingsManager as Partial<RelaySettingsManager>;
		const savedForkProvider =
			typeof sm.getWorklogForkProvider === "function" ? sm.getWorklogForkProvider() : undefined;
		const savedForkModelId =
			typeof sm.getWorklogForkModel === "function" ? sm.getWorklogForkModel() : undefined;
		if (savedForkProvider && savedForkModelId) {
			const restoredForkModel = getModel(savedForkProvider as never, savedForkModelId as never);
			if (restoredForkModel) {
				orchestrator.setForkModel(restoredForkModel);
			} else {
				// The persisted worklog fork model is no longer resolvable
				// (model removed from registry, provider renamed, etc.).
				// Surface a non-fatal diagnostic so the user sees the
				// setting was effectively dropped instead of silently
				// running against the session's main model with no feedback.
				services.diagnostics.push({
					type: "warning",
					message: `Configured worklog fork model '${savedForkProvider}/${savedForkModelId}' is no longer available; falling back to session model. Run /worklog-model to reconfigure.`,
				});
			}
		}
		const savedForkThinking =
			typeof sm.getWorklogForkThinkingLevel === "function"
				? sm.getWorklogForkThinkingLevel()
				: undefined;
		if (savedForkThinking) {
			orchestrator.setForkThinkingLevel(
				savedForkThinking as Parameters<Orchestrator["setForkThinkingLevel"]>[0],
			);
		}
		orchestratorRef.current = orchestrator;
		stateRef.current = { orchestrator, engineConfig };
		await orchestrator.restore();
		return {
			...created,
			services,
			diagnostics: services.diagnostics,
		};
	};
}

export async function createRelayRuntime(options?: {
	cwd?: string;
	agentDir?: string;
	sessionManager?: SessionManager;
}) {
	const cwd = options?.cwd ?? process.cwd();
	const agentDir = options?.agentDir ?? getAgentDir();
	const sessionManager = options?.sessionManager ?? SessionManager.continueRecent(cwd);
	return createAgentSessionRuntime(createRelayRuntimeFactory(agentDir), {
		cwd,
		agentDir,
		sessionManager,
	});
}

export async function createRelayInteractiveRuntime(options?: {
	cwd?: string;
	agentDir?: string;
	sessionManager?: SessionManager;
}) {
	const cwd = options?.cwd ?? process.cwd();
	const agentDir = options?.agentDir ?? getAgentDir();
	const sessionManager = options?.sessionManager ?? SessionManager.create(cwd);
	const stateRef: RelayRuntimeStateRef = {};
	const runtime = await createAgentSessionRuntime(createRelayRuntimeFactory(agentDir, stateRef), {
		cwd,
		agentDir,
		sessionManager,
	});
	return new RelayRuntimeHost(runtime, stateRef) as unknown as typeof runtime;
}
