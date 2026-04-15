#!/usr/bin/env node

import { InteractiveMode, runRpcMode } from "@mariozechner/pi-coding-agent";
import { createRelayInteractiveRuntime, createRelayRuntime, parseArgs } from "./runtime.js";

const cli = parseArgs(process.argv.slice(2));

if (cli.mode === "rpc") {
	const runtime = await createRelayRuntime();
	await runRpcMode(runtime);
} else {
	const runtime = await createRelayInteractiveRuntime();
	const interactiveMode = new InteractiveMode(runtime, {
		initialMessage: cli.initialMessage,
	});
	await interactiveMode.run();
}
