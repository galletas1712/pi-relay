import { PassThrough } from "node:stream";
import { describe, expect, test } from "vitest";
import { RpcClient } from "../../../src/client/rpc/client.js";
import { encodeMessage } from "../../../src/client/rpc/wire.js";

/**
 * Build a RpcClient whose input/output streams we control directly.
 *
 * These tests deliberately do NOT spin up a matching RpcServer because the
 * behavior under test is purely client-side: how it reacts when the bytes on
 * the wire cannot be parsed. We never call `client.dispose()` (which performs
 * a round-trip "dispose" RPC and would hang without a server); the test
 * process exits once vitest tears the worker down.
 */
function makeClient(): {
	client: RpcClient;
	serverToClient: PassThrough;
	clientToServer: PassThrough;
	outgoing: string[];
} {
	const serverToClient = new PassThrough();
	const clientToServer = new PassThrough();
	const client = new RpcClient({ input: serverToClient, output: clientToServer });

	const outgoing: string[] = [];
	clientToServer.on("data", (chunk: Buffer) => {
		const text = chunk.toString("utf8");
		for (const line of text.split("\n")) {
			if (line.trim().length > 0) outgoing.push(line);
		}
	});

	return { client, serverToClient, clientToServer, outgoing };
}

async function flush(): Promise<void> {
	// One macrotask yield is enough for PassThrough's synchronous 'data'
	// handlers to fire and for the client's microtask-chained handlers to
	// settle the pending promise.
	await new Promise((r) => setImmediate(r));
}

describe("RpcClient malformed-frame resilience", () => {
	test("rejects all pending calls when the server sends an unparseable frame", async () => {
		const { client, serverToClient } = makeClient();

		// Fire two in-flight calls. Neither has a response yet.
		const promise1 = client.call("sessions.list", {});
		const promise2 = client.call("models.list", {});
		promise1.catch(() => {});
		promise2.catch(() => {});

		// Server emits malformed JSON — a double comma after a property.
		serverToClient.write('{"type":"result","id":1,,"value":{}}\n');

		await expect(promise1).rejects.toThrow(/RPC frame parse error/);
		await expect(promise2).rejects.toThrow(/RPC frame parse error/);
	});

	test("includes a truncated frame snippet in the error message", async () => {
		const { client, serverToClient } = makeClient();
		const pending = client.call("sessions.list", {});
		pending.catch(() => {});

		serverToClient.write('{"type":"result","id":1,,"value":{"sessions":[]}}\n');

		await expect(pending).rejects.toThrow(/Dropped frame:/);
		await expect(pending).rejects.toThrow(/"type":"result"/);
	});

	test("client remains functional for subsequent calls after a malformed frame", async () => {
		const { client, serverToClient, outgoing } = makeClient();

		// First call: server replies with garbage, so the promise rejects.
		const doomed = client.call("sessions.list", {});
		doomed.catch(() => {});
		serverToClient.write('{"type":"result","id":1,,"value":null}\n');
		await expect(doomed).rejects.toThrow(/RPC frame parse error/);

		// Second call: client is still alive, issues a new outbound frame.
		const recovered = client.call("auth.status", {});
		recovered.catch(() => {});
		await flush();

		const lastOut = outgoing.at(-1) ?? "";
		const outgoingFrame = JSON.parse(lastOut) as { id: number; method: string };
		expect(outgoingFrame.method).toBe("auth.status");

		serverToClient.write(
			encodeMessage({
				type: "result",
				id: outgoingFrame.id,
				value: { entries: [{ provider: "anthropic", hasCredential: true }] },
			} as any),
		);

		// client.call() returns the raw wire value (envelope unchanged).
		await expect(recovered).resolves.toEqual({
			entries: [{ provider: "anthropic", hasCredential: true }],
		});
	});

	test("well-formed result frames still resolve after a malformed frame", async () => {
		const { client, serverToClient, outgoing } = makeClient();

		const doomed = client.call("models.listAvailable", {});
		doomed.catch(() => {});
		serverToClient.write('{"type":"result","id":1,,"value":{"models":[]}}\n');
		await expect(doomed).rejects.toThrow(/RPC frame parse error/);

		const followUp = client.call("models.list", {});
		followUp.catch(() => {});
		await flush();

		const lastOut = outgoing.at(-1) ?? "";
		const outgoingFrame = JSON.parse(lastOut) as { id: number; method: string };
		expect(outgoingFrame.method).toBe("models.list");
		serverToClient.write(
			encodeMessage({
				type: "result",
				id: outgoingFrame.id,
				value: { models: [] },
			} as any),
		);
		await expect(followUp).resolves.toEqual({ models: [] });
	});
});
