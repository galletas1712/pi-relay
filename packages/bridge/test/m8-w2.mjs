// M8-W2 — workspace.* through the real bridge→host path:
// project.create with git+local workspace decls → session.create(projectId)
// materializes a real session subvolume → host runs inside it →
// workspace.list/list_dir/read_file/write_file/search/git_status/git_diff →
// kernel executes with cwd == materialized session cwd → session.delete
// destroys the subvolume. Trace: m8-w2-contract.jsonl
import { execFileSync } from "node:child_process";
import { existsSync, mkdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { BridgeClient, assert } from "./client.mjs";
import { BRIDGE_URL, ORIGIN, TRACES_DIR, authToken, bridgePid, psql } from "./harness.mjs";

const RUN = join(new URL(".", import.meta.url).pathname, ".tmp-m8-w2");
rmSync(RUN, { recursive: true, force: true });
mkdirSync(RUN, { recursive: true });

const sh = (cmd, args, cwd) =>
	execFileSync(cmd, ["-c", "commit.gpgsign=false", ...args].filter((a, i) => !(cmd !== "git" && i < 2)), {
		cwd,
		encoding: "utf8",
		env: { ...process.env, GIT_TERMINAL_PROMPT: "0", GIT_AUTHOR_NAME: "t", GIT_AUTHOR_EMAIL: "t@t", GIT_COMMITTER_NAME: "t", GIT_COMMITTER_EMAIL: "t@t" },
	}).trim();

// fixtures: git "remote" + local source dir
const REMOTE = join(RUN, "remote");
mkdirSync(REMOTE, { recursive: true });
sh("git", ["init", "-b", "main"], REMOTE);
writeFileSync(join(REMOTE, "README.md"), "w2 git readme\n");
writeFileSync(join(REMOTE, "AGENTS.md"), "W2-GIT-INSTRUCTIONS\n");
sh("git", ["add", "-A"], REMOTE);
sh("git", ["commit", "-m", "init"], REMOTE);

const LOCAL_SRC = join(RUN, "local-src");
mkdirSync(join(LOCAL_SRC, "notes"), { recursive: true });
writeFileSync(join(LOCAL_SRC, "notes", "n.md"), "w2 local note\n");
writeFileSync(join(LOCAL_SRC, "AGENTS.md"), "W2-LOCAL-INSTRUCTIONS\n");

const trace = join(TRACES_DIR, "m8-w2-contract.jsonl");
const c = new BridgeClient(BRIDGE_URL, authToken(), ORIGIN, trace);
await c.connect();
const idem = `m8w2-${Date.now()}`;

// project.create with workspace decls
const proj = await c.call("project.create", {
	name: "w2-project",
	workspaces: [
		{ kind: "git", workspaceDir: "repo", remoteUrl: REMOTE, remoteBranch: "main" },
		{ kind: "local", workspaceDir: "docs", sourcePath: LOCAL_SRC },
	],
	idempotencyKey: `${idem}-proj`,
});
assert(!proj.error && proj.result.projectId, `project.create ok (${proj.result?.projectId})`);
const projectId = proj.result.projectId;

// session.create inherits the project's workspace decls
const created = await c.call("session.create", { projectId, name: "w2", idempotencyKey: `${idem}-sess` });
assert(!created.error, `session.create ok (${created.result?.sessionId})`);
const sessionId = created.result.sessionId;

const att = await c.call("session.attach", { id: sessionId, fromSeq: 0 });
assert(!att.error, `attached (headSeq=${att.result?.headSeq})`);

const st = await c.call("session.getState", { sessionId });
assert(!st.error && st.result.hostAlive === true, "host alive in materialized session");
assert(st.result.projectId === projectId, "getState reports projectId");
const cwd = st.result.cwd;
assert(cwd.includes("workspace-state/sessions/") && cwd.endsWith(`${sessionId}/cwd`), `cwd is the materialized session cwd (${cwd})`);
assert(Array.isArray(st.result.workspaces) && st.result.workspaces.length === 2, "getState reports 2 workspaces");

// PG holds project_id + workspaces
const pg = psql(`select project_id, jsonb_array_length(workspaces) from sessions where id='${sessionId}'`);
assert(pg.startsWith(projectId), `PG sessions row has project_id + 2 workspaces (${pg})`);

// workspace.list
const list = await c.call("workspace.list", { sessionId });
assert(!list.error && list.result.managed === true && list.result.btrfs === true, "workspace.list: managed, btrfs");
assert(list.result.workspaces.map((w) => w.workspaceDir).sort().join(",") === "docs,repo", "workspace.list roots repo+docs");

// list_dir / read_file
const root = await c.call("workspace.list_dir", { sessionId, path: "" });
assert(root.result.entries.map((e) => e.name).sort().join(",") === "docs,repo", "list_dir root shows repo+docs");
const readme = await c.call("workspace.read_file", { sessionId, path: "repo/README.md" });
assert(Buffer.from(readme.result.contentBase64, "base64").toString() === "w2 git readme\n", "read_file repo/README.md");
const localAgents = await c.call("workspace.read_file", { sessionId, path: "docs/AGENTS.md" });
assert(Buffer.from(localAgents.result.contentBase64, "base64").toString() === "W2-LOCAL-INSTRUCTIONS\n", "read_file docs/AGENTS.md (local workspace materialized)");

// write_file + search + git_status + git_diff
const wr = await c.call("workspace.write_file", {
	sessionId,
	path: "repo/bridge-note.txt",
	contentBase64: Buffer.from("bridge wrote this w2-marker\n").toString("base64"),
});
assert(!wr.error && wr.result.bytes > 0, "write_file repo/bridge-note.txt");
const onDisk = readFileSync(join(cwd, "repo", "bridge-note.txt"), "utf8");
assert(onDisk.includes("w2-marker"), "write landed in the materialized subvolume on disk");

const hits = await c.call("workspace.search", { sessionId, query: "w2-marker", fixedString: true });
assert(hits.result.matches.length === 1 && hits.result.matches[0].path === "repo/bridge-note.txt", "search finds the written file");

const gs = await c.call("workspace.git_status", { sessionId, against: "working_tree" });
const repoRoot = gs.result.roots.find((r) => r.workspaceDir === "repo");
assert(repoRoot.entries.some((e) => e.path === "repo/bridge-note.txt" && e.status === "untracked"), "git_status sees untracked note");
const gd = await c.call("workspace.git_diff", { sessionId, path: "repo/bridge-note.txt" });
assert(gd.result.diff.includes("+bridge wrote this w2-marker"), "git_diff shows the new file content");
const gdAll = await c.call("workspace.git_diff", { sessionId });
assert(gdAll.result.diff.includes("bridge-note.txt"), "git_diff (all) includes the file");

// confinement through the contract
const esc = await c.call("workspace.read_file", { sessionId, path: "../../etc/hostname" });
assert(!!esc.error, "read_file outside the session cwd is rejected");

// host actually runs in the materialized cwd (kernel proof via GLM + ipython)
const p = await c.call("prompt.send", {
	sessionId,
	text: `Use the ipython tool: run exactly one cell: import os; print("W2-CWD=" + os.getcwd()). Then reply with exactly W2-DONE.`,
	idempotencyKey: `${idem}-prompt`,
});
assert(p.result?.accepted === true, "prompt accepted");
await c.waitIdleAfter(sessionId, c.headSeq(sessionId), 600000);
const deltas = c.events.filter((ev) => ev.event === "message.delta" && ev.data?.kind === "text").map((ev) => ev.data.delta ?? "").join("");
assert(deltas.includes("W2-DONE"), "model finished (W2-DONE)");
const toolOut = c.events
	.filter((ev) => ev.event === "tool.exec")
	.map((ev) => JSON.stringify(ev.data))
	.join(" ");
assert(toolOut.includes(`W2-CWD=${cwd}`), `kernel cwd == materialized session cwd (saw W2-CWD=${cwd} in tool output)`);

// session.delete destroys the subvolume + PG rows
const del = await c.call("session.delete", { id: sessionId });
assert(del.result?.deleted === true, "session.delete ok");
assert(!existsSync(join(cwd, "..")), "session workspace root removed after delete");
const gone = psql(`select count(*) from sessions where id='${sessionId}'`);
assert(gone === "0", "PG session row deleted");

// project.delete clears sessions.project_id (SET NULL) — create+delete a probe session
const probe = await c.call("session.create", { projectId, name: "w2-probe", idempotencyKey: `${idem}-probe` });
const pd = await c.call("project.delete", { id: projectId });
assert(pd.result?.deleted === true, "project.delete ok");
const pn = psql(`select coalesce(project_id,'NULL') from sessions where id='${probe.result.sessionId}'`);
assert(pn === "NULL", "sessions.project_id set to NULL on project delete");
await c.call("session.delete", { id: probe.result.sessionId });

c.close();
rmSync(RUN, { recursive: true, force: true });
console.log("M8-W2 PASS");
process.exit(0);
