#!/usr/bin/env node

import { InteractiveMode, runRpcMode } from "@mariozechner/pi-coding-agent";
import { createRelayRuntime, parseArgs } from "./runtime.js";

const cli = parseArgs(process.argv.slice(2));
const runtime = await createRelayRuntime();

if (cli.mode === "rpc") {
	await runRpcMode(runtime);
} else {
	const interactiveMode = new InteractiveMode(runtime, {
		initialMessage: cli.initialMessage,
	});
	await interactiveMode.run();
}
