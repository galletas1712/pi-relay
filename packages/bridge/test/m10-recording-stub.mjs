// M10b: loopback OpenAI-/Anthropic-shaped provider stub. Records every request
// (method/url/body — never headers, so nothing credential-shaped can land on
// disk) to a JSONL file and answers with a minimal spec-shaped SSE stream.
// Lets the payload test diff provider cache fields end-to-end through a real
// `pi --mode rpc` host with zero network and zero credentials.
import http from "node:http";
import { appendFileSync } from "node:fs";

export function startRecordingStub(recordPath) {
	const server = http.createServer((req, res) => {
		let body = "";
		req.on("data", (c) => (body += c));
		req.on("end", () => {
			let payload = null;
			try { payload = JSON.parse(body); } catch { /* recorded as null */ }
			try {
				appendFileSync(recordPath, JSON.stringify({ ts: new Date().toISOString(), method: req.method, url: req.url, body: payload }) + "\n");
			} catch { /* recording must never break the stub */ }
			try {
				if (req.method === "POST" && req.url?.endsWith("/chat/completions")) return openaiSse(res, payload);
				if (req.method === "POST" && req.url?.endsWith("/messages")) return anthropicSse(res);
				res.writeHead(404);
				res.end("not found");
			} catch (e) {
				res.writeHead(500);
				res.end(String(e));
			}
		});
	});
	return new Promise((resolve, reject) => {
		server.once("error", reject);
		server.listen(0, "127.0.0.1", () => {
			resolve({ port: server.address().port, close: () => new Promise((r) => server.close(r)) });
		});
	});
}

function sse(res, obj) {
	res.write(`data: ${JSON.stringify(obj)}\n\n`);
}

// Mirrors shim-proxy.mjs's synth (proven against pi in M1): content chunk(s),
// finish chunk, usage chunk only when include_usage was requested, [DONE].
function openaiSse(res, payload) {
	res.writeHead(200, { "Content-Type": "text/event-stream", "Cache-Control": "no-cache", Connection: "keep-alive" });
	const base = { id: "chatcmpl-m10", object: "chat.completion.chunk", created: 1_700_000_000, model: payload?.model ?? "test-model" };
	sse(res, { ...base, choices: [{ index: 0, delta: { role: "assistant", content: "CACHE-PROBE-OK" }, finish_reason: null }] });
	sse(res, { ...base, choices: [{ index: 0, delta: {}, finish_reason: "stop" }] });
	if (payload?.stream === true && payload?.stream_options?.include_usage === true) {
		sse(res, {
			...base,
			choices: [],
			usage: { prompt_tokens: 123, completion_tokens: 3, total_tokens: 126, prompt_tokens_details: { cached_tokens: 0 } },
		});
	}
	res.write("data: [DONE]\n\n");
	res.end();
}

// Minimal Anthropic Messages SSE set (shape taken from pi-ai's own
// anthropic-sse-parsing.test.ts in pi-mono).
function anthropicSse(res) {
	const usage = { input_tokens: 12, output_tokens: 0, cache_read_input_tokens: 0, cache_creation_input_tokens: 0 };
	const events = [
		{ event: "message_start", data: { type: "message_start", message: { id: "msg_m10", usage } } },
		{ event: "content_block_start", data: { type: "content_block_start", index: 0, content_block: { type: "text", text: "" } } },
		{ event: "content_block_delta", data: { type: "content_block_delta", index: 0, delta: { type: "text_delta", text: "CACHE-PROBE-OK" } } },
		{ event: "content_block_stop", data: { type: "content_block_stop", index: 0 } },
		{ event: "message_delta", data: { type: "message_delta", delta: { stop_reason: "end_turn" }, usage: { ...usage, output_tokens: 3 } } },
		{ event: "message_stop", data: { type: "message_stop" } },
	];
	res.writeHead(200, { "Content-Type": "text/event-stream" });
	for (const { event, data } of events) res.write(`event: ${event}\ndata: ${JSON.stringify(data)}\n\n`);
	res.end();
}
