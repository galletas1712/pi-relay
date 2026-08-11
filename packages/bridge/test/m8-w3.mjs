// M8-W3 — AGENTS.md from ALL workspace dirs lands in the system prompt:
// project with a git workspace dir and a local workspace dir, each carrying a
// marker AGENTS.md; the host (spawned with PI_RELAY_WORKSPACE_DIRS) must load
// both via prime-harness loadContextFiles; the model must be able to quote
// both markers. Trace: m8-w3-agents.jsonl
import { execFileSync } from "node:child_process";
import { mkdirSync, rmSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { BridgeClient, collectText, assert } from "./client.mjs";
import { BRIDGE_URL, ORIGIN, TRACES_DIR, authToken } from "./harness.mjs";

const RUN = join(new URL(".", import.meta.url).pathname, ".tmp-m8-w3");
rmSync(RUN, { recursive: true, force: true });
mkdirSync(RUN, { recursive: true });

const sh = (cmd, args, cwd) =>
	execFileSync(cmd, ["-c", "commit.gpgsign=false", ...args].filter((a, i) => !(cmd !== "git" && i < 2)), {
		cwd,
		encoding: "utf8",
		env: { ...process.env, GIT_TERMINAL_PROMPT: "0", GIT_AUTHOR_NAME: "t", GIT_AUTHOR_EMAIL: "t@t", GIT_COMMITTER_NAME: "t", GIT_COMMITTER_EMAIL: "t@t" },
	}).trim();

const REMOTE = join(RUN, "remote");
mkdirSync(REMOTE, { recursive: true });
sh("git", ["init", "-b", "main"], REMOTE);
writeFileSync(join(REMOTE, "AGENTS.md"), "Repo instructions: the git marker is W3-GIT-MARKER-7Q2.\n");
writeFileSync(join(REMOTE, "x.ts"), "export {}\n");
sh("git", ["add", "-A"], REMOTE);
sh("git", ["commit", "-m", "init"], REMOTE);

const LOCAL_SRC = join(RUN, "local-src");
mkdirSync(LOCAL_SRC, { recursive: true });
writeFileSync(join(LOCAL_SRC, "AGENTS.md"), "Docs instructions: the local marker is W3-LOCAL-MARKER-Z4P.\n");

const c = new BridgeClient(BRIDGE_URL, authToken(), ORIGIN, join(TRACES_DIR, "m8-w3-agents.jsonl"));
await c.connect();
const idem = `m8w3-${Date.now()}`;

const proj = await c.call("project.create", {
	name: "w3-project",
	workspaces: [
		{ kind: "git", workspaceDir: "repo", remoteUrl: REMOTE, remoteBranch: "main" },
		{ kind: "local", workspaceDir: "docs", sourcePath: LOCAL_SRC },
	],
	idempotencyKey: `${idem}-proj`,
});
assert(!proj.error, "project.create ok");
const created = await c.call("session.create", { projectId: proj.result.projectId, name: "w3", idempotencyKey: `${idem}-sess` });
const sessionId = created.result.sessionId;
assert(!created.error, `session.create ok (${sessionId})`);
await c.call("session.attach", { id: sessionId, fromSeq: 0 });

const p = await c.call("prompt.send", {
	sessionId,
	text: "Standing instructions were loaded from AGENTS.md files in my project context. Without using any tool: quote the git marker (format W3-GIT-MARKER-...) and the local marker (format W3-LOCAL-MARKER-...), one per line, then a final line saying exactly W3-DONE.",
	idempotencyKey: `${idem}-prompt`,
});
assert(p.result?.accepted === true, "prompt accepted");
await c.waitIdleAfter(sessionId, c.headSeq(sessionId), 300000);
const text = collectText(c.events, sessionId);
assert(text.includes("W3-GIT-MARKER-7Q2"), "model quotes the git workspace AGENTS.md marker");
assert(text.includes("W3-LOCAL-MARKER-Z4P"), "model quotes the local workspace AGENTS.md marker");
assert(text.includes("W3-DONE"), "model finished");

await c.call("session.delete", { id: sessionId });
await c.call("project.delete", { id: proj.result.projectId });
c.close();
rmSync(RUN, { recursive: true, force: true });
console.log("M8-W3 PASS");
process.exit(0);
