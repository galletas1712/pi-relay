// M11c: empty-bubble fix — rebuildBranchTranscript must skip assistant
// message blocks that carry no text AND no thinking (tool-call-only steps);
// their toolCall parts still emit tool blocks. User messages always emit.
// Self-contained: writes a synthetic pi session JSONL, no bridge/PG.
//   node --test test/m11c-bubbles.test.mjs
import { test } from "node:test";
import assert from "node:assert/strict";
import { existsSync, mkdtempSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const { rebuildBranchTranscript } = await import(path.resolve(here, "../src/transcript.ts"));

function writeSession(records) {
	const dir = mkdtempSync(path.join(tmpdir(), "m11c-bubbles-"));
	const file = path.join(dir, "session.jsonl");
	writeFileSync(file, records.map((r) => JSON.stringify(r)).join("\n") + "\n");
	return file;
}

function message(id, parentId, role, content) {
	return { type: "message", id, parentId, timestamp: "2026-08-10T17:00:00.000Z", message: { role, content } };
}

test("assistant message with text+thinking both empty emits no message block, tool blocks still emit", () => {
	const file = writeSession([
		message("u1", null, "user", [{ type: "text", text: "go" }]),
		// tool-call-only assistant step (no text, no thinking) — the empty bubble source
		message("a1", "u1", "assistant", [
			{ type: "toolCall", id: "call_1", name: "ipython", arguments: { code: "print(1)" } },
		]),
		{ type: "message", id: "t1", parentId: "a1", timestamp: "2026-08-10T17:00:01.000Z", message: { role: "toolResult", toolCallId: "call_1", toolName: "ipython", content: [{ type: "text", text: "1\n" }] } },
		message("a2", "t1", "assistant", [{ type: "text", text: "done" }]),
	]);
	const { blocks } = rebuildBranchTranscript(file);
	const messages = blocks.filter((b) => b.kind === "message");
	assert.deepEqual(messages.map((m) => m.id), ["u1", "a2"]);
	const tools = blocks.filter((b) => b.kind === "tool");
	assert.equal(tools.length, 1);
	assert.equal(tools[0].toolCallId, "call_1");
	assert.equal(tools[0].result, "1\n");
	assert.equal(tools[0].done, true);
});

test("assistant message with whitespace-only text but thinking still emits (adapter drops it instead)", () => {
	const file = writeSession([
		message("u1", null, "user", [{ type: "text", text: "go" }]),
		message("a1", "u1", "assistant", [
			{ type: "thinking", thinking: "plan the call" },
			{ type: "text", text: "\n\n" },
			{ type: "toolCall", id: "call_1", name: "ipython", arguments: { code: "x = 1" } },
		]),
		message("a2", "a1", "assistant", [{ type: "text", text: "done" }]),
	]);
	const { blocks } = rebuildBranchTranscript(file);
	const messages = blocks.filter((b) => b.kind === "message");
	assert.deepEqual(messages.map((m) => m.id), ["u1", "a1", "a2"]);
	assert.equal(messages[1].thinking, "plan the call");
	assert.equal(blocks.filter((b) => b.kind === "tool").length, 1);
});

test("user messages always emit, even when empty", () => {
	const file = writeSession([
		message("u1", null, "user", []),
		message("a1", "u1", "assistant", [{ type: "text", text: "hi" }]),
	]);
	const { blocks } = rebuildBranchTranscript(file);
	const messages = blocks.filter((b) => b.kind === "message");
	assert.deepEqual(messages.map((m) => m.id), ["u1", "a1"]);
	assert.equal(messages[0].text, "");
});

test("real dogfood session (empty-bubble report) yields no empty assistant message blocks", (t) => {
	const repoRoot = path.resolve(here, "../../..");
	const file = path.join(
		repoRoot,
		".pi/dogfood/bridge-data/sessions/2026-08-10T17-26-47-305Z_session_b09254c2-200b-483d-9abe-3ccf74d706b0.jsonl",
	);
	if (!existsSync(file)) {
		t.skip("dogfood session file not present on this host");
		return;
	}
	const { blocks } = rebuildBranchTranscript(file);
	const assistants = blocks.filter((b) => b.kind === "message" && b.role === "assistant");
	assert.ok(assistants.length > 0, "expected assistant messages in the dogfood session");
	for (const a of assistants) {
		assert.ok(a.text.trim().length > 0 || a.thinking.trim().length > 0, `empty assistant message block ${a.id}`);
	}
});
