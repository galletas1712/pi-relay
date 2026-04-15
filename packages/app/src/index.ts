#!/usr/bin/env node

/**
 * pi-relay entry point
 *
 * Wires the coding-agent SDK with our modified agent-core (workspace override)
 * and starts InteractiveMode (TUI). For Phase 1 testing, this verifies that
 * the base TUI works with our agent-core fork. Later phases add the
 * orchestrator extension for multi-agent coordination.
 */

import {
	type CreateAgentSessionRuntimeFactory,
	createAgentSessionFromServices,
	createAgentSessionRuntime,
	createAgentSessionServices,
	getAgentDir,
	InteractiveMode,
	SessionManager,
} from "@mariozechner/pi-coding-agent";

const cwd = process.cwd();
const agentDir = getAgentDir();

const createRuntime: CreateAgentSessionRuntimeFactory = async ({ cwd, sessionManager, sessionStartEvent }) => {
	const services = await createAgentSessionServices({ cwd });
	const created = await createAgentSessionFromServices({
		services,
		sessionManager,
		sessionStartEvent,
	});
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

const interactiveMode = new InteractiveMode(runtime, {
	initialMessage: process.argv[2],
});

await interactiveMode.run();
