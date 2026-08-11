// Minimal MCP stdio server (newline-delimited JSON-RPC per the stdio
// transport): initialize / notifications / tools/list / tools/call.
const readline = require("node:readline");
const rl = readline.createInterface({ input: process.stdin });
rl.on("line", (line) => {
	let msg;
	try {
		msg = JSON.parse(line);
	} catch {
		return;
	}
	if (msg.method === "initialize") {
		return send({ jsonrpc: "2.0", id: msg.id, result: { protocolVersion: msg.params?.protocolVersion ?? "2025-03-26", capabilities: { tools: {} }, serverInfo: { name: "mock-stdio", version: "1.0.0" } } });
	}
	if (String(msg.method ?? "").startsWith("notifications/")) return;
	if (msg.method === "tools/list") {
		return send({
			jsonrpc: "2.0",
			id: msg.id,
			result: {
				tools: [
					{ name: "stdio.ping", description: "Ping the stdio mock", inputSchema: { type: "object", properties: {} } },
					{ name: "stdio.hidden", description: "Not enabled in tests", inputSchema: { type: "object", properties: {} } },
				],
			},
		});
	}
	if (msg.method === "tools/call") {
		const { name } = msg.params ?? {};
		if (name === "stdio.ping") return send({ jsonrpc: "2.0", id: msg.id, result: { content: [{ type: "text", text: "STDIO-PONG" }], isError: false } });
		return send({ jsonrpc: "2.0", id: msg.id, result: { content: [{ type: "text", text: `unknown tool ${name}` }], isError: true } });
	}
	if (msg.id !== undefined) send({ jsonrpc: "2.0", id: msg.id, error: { code: -32601, message: "method not found" } });
});
function send(obj) {
	process.stdout.write(JSON.stringify(obj) + "\n");
}
