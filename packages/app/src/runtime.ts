import {
	createApplyPatchToolDefinition,
	type CreateAgentSessionRuntimeFactory,
	createAgentSessionFromServices,
	createAgentSessionRuntime,
	createAgentSessionServices,
	createBashToolDefinition,
	createEditToolDefinition,
	createFileAccessTracker,
	createReadToolDefinition,
	createWriteToolDefinition,
	getAgentDir,
	SessionManager,
	type AgentSessionServices,
	type ToolDefinition,
} from "@mariozechner/pi-coding-agent";
import {
	createMessageTool,
	createOrchestratorExtension,
	Orchestrator,
	createRelaySessionFactory,
	createSpawnTool,
} from "@pi-relay/orchestrator";
import { RelayRuntimeHost, type RelayRuntimeStateRef } from "./relay-runtime-host.js";

const RELAY_BASE_TOOL_NAMES = ["read", "bash", "edit", "apply_patch", "write"];

const RELAY_APPEND_SYSTEM_PROMPT = `Relay tool usage:
- Use read instead of cat, head, tail, or sed for reading files.
- Use apply_patch for multi-file or diff-shaped changes to existing files.
- Use edit for precise replacements inside one existing file.
- Use write only for new files or complete rewrites.
- Do not use bash to read or edit files when dedicated tools are available.
- After apply_patch succeeds, do not immediately re-read the same file unless you need verification or nearby context.`;

function createRelayBaseToolDefinitionsFactory(
	cwd: string,
	settingsManager: AgentSessionServices["settingsManager"],
): () => ToolDefinition<any, any, any>[] {
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
	const orchestratorUiRef: { cleanup?: () => void; sessionId?: string } = {};
	return async ({ cwd, sessionManager, sessionStartEvent }) => {
		const orchestratorRef: { current?: Orchestrator } = {};
		const services = await createAgentSessionServices({
			cwd,
			agentDir,
			resourceLoaderOptions: {
				appendSystemPrompt: [RELAY_APPEND_SYSTEM_PROMPT],
				extensionFactories: [createOrchestratorExtension(orchestratorRef, orchestratorUiRef)],
			},
		});
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
			toolNames: RELAY_BASE_TOOL_NAMES,
			baseToolDefinitionsFactory: rootBaseToolDefinitionsFactory,
			customTools: rootTools,
		});
		const orchestrator = new Orchestrator({
			rootSession: created.session,
			sessionFactory: createRelaySessionFactory({
				services,
				defaultSessionDir: sessionManager.getSessionDir(),
				baseToolNames: RELAY_BASE_TOOL_NAMES,
				createSessionBaseToolDefinitionsFactory,
			}),
		});
		orchestratorRef.current = orchestrator;
		stateRef.current = { orchestrator };
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
	const sessionManager = options?.sessionManager ?? SessionManager.continueRecent(cwd);
	const stateRef: RelayRuntimeStateRef = {};
	const runtime = await createAgentSessionRuntime(createRelayRuntimeFactory(agentDir, stateRef), {
		cwd,
		agentDir,
		sessionManager,
	});
	return new RelayRuntimeHost(runtime, stateRef) as unknown as typeof runtime;
}
