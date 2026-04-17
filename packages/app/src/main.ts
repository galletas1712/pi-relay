import { InteractiveMode, LocalClient, runRpcMode } from "@pi-relay/coding-agent";
import { createRelayInteractiveRuntime, createRelayRuntime, parseArgs } from "./runtime.js";

const cli = parseArgs(process.argv.slice(2));

if (cli.mode === "rpc") {
	const runtime = await createRelayRuntime();
	await runRpcMode(runtime);
} else {
	const runtime = await createRelayInteractiveRuntime();
	const client = new LocalClient(runtime);
	const interactiveMode = new InteractiveMode(client, {
		initialMessage: cli.initialMessage,
	});
	await interactiveMode.run();
}
