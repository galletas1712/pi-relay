// V2 consumer-load tests (M10a): REAL consumers parse migrated artifacts.
//  a) bridge transcript walker rebuilds a migrated session file
//  b) bridge parseMcpConfig parses the migrated mcp.toml
//  c) prime-harness loadHarnessState loads the migrated harness_state.json
//  d) prime-harness discoverRoles + loadHarnessSkills over the migrated agent dir
// Usage: node test/v2-consumers.mjs <outRoot> [sessionFile]
import { readFileSync, readdirSync } from "node:fs";
import { join, resolve } from "node:path";
import { rebuildTranscript } from "../../bridge/src/transcript.ts";
import { parseMcpConfig } from "../../bridge/src/mcp/config.ts";
import { loadHarnessState } from "../../../extensions/prime-harness/src/store.ts";
import { discoverRoles } from "../../../extensions/prime-harness/src/roles.ts";
import { loadHarnessSkills } from "../../../extensions/prime-harness/src/skills.ts";

const outRoot = resolve(process.argv[2] ?? "out");
let sessionFile = process.argv[3] ? resolve(process.argv[3]) : null;
if (!sessionFile) {
	// pick a session with a compaction + tool calls for maximal coverage
	const dir = join(outRoot, "sessions");
	const files = readdirSync(dir).filter((f) => f.endsWith(".jsonl"));
	for (const f of files) {
		const txt = readFileSync(join(dir, f), "utf8");
		if (txt.includes('"type":"compaction"') && txt.includes('"role":"toolResult"') && txt.includes("pi-relay-daemon-observation")) {
			sessionFile = join(dir, f);
			break;
		}
	}
	sessionFile ??= join(dir, files[0]);
}
let failures = 0;
const check = (name, ok, detail) => {
	console.log(`${ok ? "PASS" : "FAIL"} ${name}${detail ? ` — ${detail}` : ""}`);
	if (!ok) failures += 1;
};

// (a) bridge transcript walker
{
	const blocks = rebuildTranscript(sessionFile);
	const msgs = blocks.filter((b) => b.kind === "message");
	const tools = blocks.filter((b) => b.kind === "tool");
	// note: the walker projects message/tool blocks only; compaction entries are
	// branch links (skipped from the web projection) — the check is that a file
	// WITH compactions + tools rebuilds cleanly with text on the active branch.
	check(
		"walker.rebuildTranscript",
		msgs.length > 0 && tools.length > 0,
		`file=${sessionFile.split("/").pop()} blocks=${blocks.length} messages=${msgs.length} tools=${tools.length}`,
	);
	const withText = msgs.filter((m) => typeof m.text === "string" && m.text.length > 0);
	check("walker.message-text", withText.length > 0, `messages with text=${withText.length}`);
}

// (b) mcp.toml via bridge parser
{
	const cfg = parseMcpConfig(readFileSync(join(outRoot, "mcp.toml"), "utf8"));
	const ids = cfg.servers instanceof Map ? [...cfg.servers.keys()] : Object.keys(cfg.servers ?? {});
	check("mcp.parseMcpConfig", ids.length === 4, `servers=[${ids.join(",")}]`);
}

// (c) harness state
{
	const st = loadHarnessState(join(outRoot, "agent", "harness"), "global");
	const n = Object.values(st.entries ?? {}).reduce((acc, m) => acc + Object.keys(m ?? {}).length, 0);
	check("harness.loadHarnessState", n >= 1, `entries=${n} schema=${st.schema}`);
}

// (d) roles + skills discovery
{
	const disc = discoverRoles({ agentDir: join(outRoot, "agent"), cwd: outRoot, bundledSkillsDir: join(outRoot, "agent", "skills") });
	const roles = Array.isArray(disc.roles) ? disc.roles : [...(disc.roles?.values?.() ?? [])];
	// pi-relay parity: source roles dir is 9 dirs but monitor/SKILL.md has name:tester,
	// which BOTH stacks reject ("directory must match name") -> 8 usable + 1 invalid.
	check("harness.discoverRoles", roles.length === 8 && (disc.invalid?.length ?? 0) === 1, `roles=${roles.length} invalid=${disc.invalid?.length ?? 0} (parity: monitor dir has name:tester in source)`);
	const skills = loadHarnessSkills(join(outRoot, "agent", "skills"), join(outRoot, "agent"));
	check("harness.loadHarnessSkills", skills.length >= 5, `skills=${skills.length} [${skills.map((s) => s.name).slice(0, 8).join(",")}]`);
}

process.exit(failures === 0 ? 0 : 1);
