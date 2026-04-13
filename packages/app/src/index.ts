#!/usr/bin/env node

import {
	type CreateAgentSessionRuntimeFactory,
	createAgentSessionFromServices,
	createAgentSessionRuntime,
	createAgentSessionServices,
	getAgentDir,
	InteractiveMode,
	runRpcMode,
	SessionManager,
	type ToolDefinition,
} from "@mariozechner/pi-coding-agent";
import {
	createMessageTool,
	createOrchestratorExtension,
	Orchestrator,
	createRelaySessionFactory,
	createSpawnTool,
} from "@pi-relay/orchestrator";

const cwd = process.cwd();
const agentDir = getAgentDir();

function parseArgs(argv: string[]) {
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

const cli = parseArgs(process.argv.slice(2));

const createRuntime: CreateAgentSessionRuntimeFactory = async ({ cwd, sessionManager, sessionStartEvent }) => {
	const orchestratorRef: { current?: Orchestrator } = {};
	const services = await createAgentSessionServices({
		cwd,
		agentDir,
		resourceLoaderOptions: {
			extensionFactories: [createOrchestratorExtension(orchestratorRef)],
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
	return {
		...created,
		services,
		diagnostics: services.diagnostics,
	};
};

const runtime = await createAgentSessionRuntime(createRuntime, {
	cwd,
	agentDir,
	sessionManager: SessionManager.create(cwd),
});

if (cli.mode === "rpc") {
	await runRpcMode(runtime);
} else {
	const interactiveMode = new InteractiveMode(runtime, {
		initialMessage: cli.initialMessage,
	});
	await interactiveMode.run();
}
