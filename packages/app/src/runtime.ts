import {
	type CreateAgentSessionRuntimeFactory,
	createAgentSessionFromServices,
	createAgentSessionRuntime,
	createAgentSessionServices,
	getAgentDir,
	SessionManager,
	type ToolDefinition,
} from "@mariozechner/pi-coding-agent";
import {
	createChildrenTool,
	createMessageTool,
	createOrchestratorExtension,
	Orchestrator,
	createRelaySessionFactory,
	createSpawnTool,
	createTerminateTool,
} from "@pi-relay/orchestrator";
import { RelayRuntimeHost, type RelayRuntimeStateRef } from "./relay-runtime-host.js";

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
			async describeChildren(agentId: string) {
				if (!orchestratorRef.current) {
					throw new Error("Orchestrator has not been initialized yet");
				}
				return orchestratorRef.current.describeChildren(agentId);
			},
			async routeMessage(fromAgentId: string, targetAgentId: string, content: string) {
				if (!orchestratorRef.current) {
					throw new Error("Orchestrator has not been initialized yet");
				}
				await orchestratorRef.current.routeMessage(fromAgentId, targetAgentId, content);
			},
			async terminateAgent(fromAgentId: string, targetAgentId: string) {
				if (!orchestratorRef.current) {
					throw new Error("Orchestrator has not been initialized yet");
				}
				await orchestratorRef.current.terminateAgent(fromAgentId, targetAgentId);
			},
		};
		const rootTools: ToolDefinition[] = [
			createSpawnTool(rootToolBridge, "root") as unknown as ToolDefinition,
			createChildrenTool(rootToolBridge, "root") as unknown as ToolDefinition,
			createMessageTool(rootToolBridge, "root") as unknown as ToolDefinition,
			createTerminateTool(rootToolBridge, "root") as unknown as ToolDefinition,
		];
		const created = await createAgentSessionFromServices({
			services,
			sessionManager,
			sessionStartEvent,
			customTools: rootTools,
		});
		const orchestrator = new Orchestrator({
			rootSession: created.session,
			sessionFactory: createRelaySessionFactory({
				services,
				defaultSessionDir: sessionManager.getSessionDir(),
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
