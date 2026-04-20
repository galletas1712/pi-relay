import {
	flushRawStdout,
	InteractiveMode,
	LocalClient,
	RpcServer,
	takeOverStdout,
} from "@pi-relay/coding-agent";
import { createRelayInteractiveRuntime, createRelayRuntime, parseArgs } from "./runtime.js";

const cli = parseArgs(process.argv.slice(2));

if (cli.mode === "rpc") {
	// Hijack stdout before constructing the RpcServer so any stray console.log in
	// the runtime call graph is redirected to stderr and cannot interleave non-JSON
	// bytes between NDJSON frames. Matches the coding-agent entry point's ordering
	// (packages/coding-agent/src/main.ts around :455).
	takeOverStdout();
	const runtime = await createRelayRuntime();
	const client = new LocalClient(runtime);
	const server = new RpcServer(client);
	await server.listen();
	// Drain any buffered trailing frames (e.g. final dispose result) before returning
	// so the pipe teardown cannot truncate the last frame. Matches print-mode's drain
	// pattern at packages/coding-agent/src/modes/print-mode.ts around :180.
	await flushRawStdout();
} else {
	const runtime = await createRelayInteractiveRuntime();
	const client = new LocalClient(runtime);
	const interactiveMode = new InteractiveMode(client, {
		initialMessage: cli.initialMessage,
	});
	await interactiveMode.run();
}
