import { InteractiveMode, LocalClient, RpcServer } from "@pi-relay/coding-agent";
import { createRelayInteractiveRuntime, createRelayRuntime, parseArgs } from "./runtime.js";

const cli = parseArgs(process.argv.slice(2));

if (cli.mode === "rpc") {
	const runtime = await createRelayRuntime();
	const client = new LocalClient(runtime);
	const server = new RpcServer(client);
	await server.listen();
} else {
	const runtime = await createRelayInteractiveRuntime();
	const client = new LocalClient(runtime);
	const interactiveMode = new InteractiveMode(client, {
		initialMessage: cli.initialMessage,
	});
	await interactiveMode.run();
}
