#!/usr/bin/env node
// M10b full-chain payload evidence: boot real `pi --mode rpc` hosts (pinned
// @earendil-works/pi-coding-agent 0.84.1) against a loopback recording stub —
// one boot per (provider-shape × retention env × compat) case — and diff the
// exact provider payloads. No network, no credentials, no bridge, no PG.
//
//   node test/m10-cache-payloads.mjs
//
// Writes normalized per-case records to
//   .pi/m1-demo/traces/m10b-prompt-builds.jsonl
// (repo traces convention) so the cache-field diff is reviewable in git.
import { spawn } from "node:child_process";
import { mkdirSync, readFileSync, writeFileSync, rmSync, readdirSync, existsSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { startRecordingStub } from "./m10-recording-stub.mjs";

const here = path.dirname(fileURLToPath(import.meta.url));
const bridgeDir = path.resolve(here, "..");
const repoRoot = path.resolve(bridgeDir, "..", "..");
const outRoot = path.join(here, "out", "m10");
const tracesPath = path.join(repoRoot, ".pi", "m1-demo", "traces", "m10b-prompt-builds.jsonl");

// Same resolution as packages/bridge/src/config.ts (workspace-hoisting safe).
const piEntry = fileURLToPath(import.meta.resolve("@earendil-works/pi-coding-agent"));
const piCli = path.join(path.dirname(piEntry), "cli.js");

const PROMPT = "Reply with exactly: CACHE-PROBE-OK";
const ZERO_COST = { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 };

function openaiProvider(port, compat) {
	return {
		baseUrl: `http://127.0.0.1:${port}/v1`,
		api: "openai-completions",
		apiKey: "test-key",
		authHeader: true,
		...(compat ? { compat } : {}),
		models: [{ id: "test-glm", name: "Test GLM", reasoning: false, input: ["text"], contextWindow: 128000, maxTokens: 1024, cost: ZERO_COST }],
	};
}
function anthropicProvider(port) {
	return {
		baseUrl: `http://127.0.0.1:${port}`,
		api: "anthropic-messages",
		apiKey: "test-key",
		models: [{ id: "claude-test", name: "Test Claude", reasoning: false, input: ["text"], contextWindow: 200000, maxTokens: 1024, cost: ZERO_COST }],
	};
}

const CASES = [
	{ name: "openai-default", family: "openai", compat: null, retentionEnv: null },
	{ name: "openai-long", family: "openai", compat: null, retentionEnv: "long" },
	{ name: "openai-long-compat", family: "openai", compat: { supportsLongCacheRetention: false }, retentionEnv: "long" },
	{ name: "anthropic-default", family: "anthropic", compat: null, retentionEnv: null },
	{ name: "anthropic-long", family: "anthropic", compat: null, retentionEnv: "long" },
];

function writeAgentDir(caseDir, c, port) {
	const agentDir = path.join(caseDir, "agent");
	mkdirSync(agentDir, { recursive: true });
	const providerName = c.family === "openai" ? "rec-openai" : "rec-anthropic";
	const provider = c.family === "openai" ? openaiProvider(port, c.compat) : anthropicProvider(port);
	const modelId = c.family === "openai" ? "test-glm" : "claude-test";
	writeFileSync(path.join(agentDir, "settings.json"), JSON.stringify({ defaultProvider: providerName, defaultModel: modelId }, null, 2));
	writeFileSync(path.join(agentDir, "models.json"), JSON.stringify({ providers: { [providerName]: provider } }, null, 2));
	writeFileSync(path.join(agentDir, "auth.json"), "{}");
	return { agentDir, providerName, modelId };
}

function runHost(caseDir, agentDir, retentionEnv) {
	const sessionsDir = path.join(caseDir, "sessions");
	mkdirSync(sessionsDir, { recursive: true });
	const env = { ...process.env, PI_CODING_AGENT_DIR: agentDir };
	delete env.PI_CACHE_RETENTION; // default cases must not inherit a caller's value
	if (retentionEnv) env.PI_CACHE_RETENTION = retentionEnv;
	const child = spawn(process.execPath, [piCli, "--mode", "rpc", "--session-dir", sessionsDir], {
		env,
		stdio: ["pipe", "pipe", "pipe"],
	});
	let buf = "";
	let errBuf = "";
	const pending = new Map();
	let settled = false;
	let sawAssistantText = "";
	return new Promise((resolve, reject) => {
		const timer = setTimeout(() => reject(new Error("timeout waiting for agent_settled")), 90_000);
		const finish = (fn, val) => {
			clearTimeout(timer);
			try { child.stdin.end(); } catch {}
			setTimeout(() => { try { child.kill("SIGKILL"); } catch {} }, 300);
			fn(val);
		};
		const handleLine = async (line) => {
			let obj;
			try { obj = JSON.parse(line); } catch { return; }
			if (obj.type === "response" && obj.id && pending.has(obj.id)) {
				pending.get(obj.id)(obj);
				pending.delete(obj.id);
			}
			if (obj.type === "agent_settled" && !settled) {
				settled = true;
				const id = "final-get-messages";
				const respP = new Promise((res) => pending.set(id, res));
				child.stdin.write(JSON.stringify({ id, type: "get_messages" }) + "\n");
				const resp = await respP;
				const messages = resp.data?.messages ?? resp.messages ?? [];
				let usage = null;
				for (let i = messages.length - 1; i >= 0; i--) {
					const m = messages[i];
					if (m.role === "assistant") {
						const parts = Array.isArray(m.content) ? m.content : [];
						sawAssistantText = parts.filter((p) => p.type === "text").map((p) => p.text).join("\n");
						usage = m.usage ?? null;
						break;
					}
				}
				finish(resolve, { finalText: sawAssistantText, usage });
			}
		};
		child.stdout.on("data", (chunk) => {
			buf += chunk.toString("utf8");
			let idx;
			while ((idx = buf.indexOf("\n")) >= 0) {
				let line = buf.slice(0, idx);
				buf = buf.slice(idx + 1);
				if (line.endsWith("\r")) line = line.slice(0, -1);
				if (line.trim()) void handleLine(line).catch((e) => finish(reject, e));
			}
		});
		child.stderr.on("data", (chunk) => { errBuf += chunk.toString("utf8"); });
		child.on("exit", (code) => { if (!settled) finish(reject, new Error(`pi exited early (code ${code})\nstderr tail:\n${errBuf.slice(-2000)}`)); });
		child.on("error", (err) => finish(reject, err));
		child.stdin.write(JSON.stringify({ id: "req-1", type: "prompt", message: PROMPT }) + "\n");
	});
}

function readRecordings(file) {
	if (!existsSync(file)) return [];
	return readFileSync(file, "utf8").split("\n").filter(Boolean).map((l) => JSON.parse(l));
}

function cacheFields(rec) {
	const b = rec?.body ?? {};
	const ccSummary = (blocks) => {
		const withCc = (blocks ?? []).filter((x) => x && typeof x === "object" && x.cache_control);
		return { count: withCc.length, ttls: [...new Set(withCc.map((x) => x.cache_control.ttl ?? "(none)"))] };
	};
	// Shape by endpoint, not body keys: /messages = anthropic-messages,
	// /chat/completions = openai-completions (both carry a messages array).
	if (rec?.url?.endsWith("/messages")) {
		// anthropic-messages shape
		const sysBlocks = Array.isArray(b.system) ? b.system : [];
		const users = (b.messages ?? []).filter((m) => m.role === "user");
		const lastUser = users[users.length - 1];
		const lastUserBlocks = Array.isArray(lastUser?.content) ? lastUser.content : [];
		return {
			prompt_cache_key: null,
			prompt_cache_retention: null,
			system_cc: ccSummary(sysBlocks),
			tool_cc: ccSummary(b.tools),
			last_user_cc: ccSummary(lastUserBlocks),
		};
	}
	// openai-completions shape
	return {
		prompt_cache_key: b.prompt_cache_key ?? null,
		prompt_cache_retention: b.prompt_cache_retention ?? null,
		system_cc: { count: 0, ttls: [] },
		tool_cc: { count: 0, ttls: [] },
		last_user_cc: { count: 0, ttls: [] },
	};
}

function assertCase(name, cond, detail) {
	if (!cond) {
		console.error(`FAIL [${name}]: ${detail}`);
		process.exitCode = 1;
		return false;
	}
	console.error(`ok   [${name}] ${detail}`);
	return true;
}

async function main() {
	rmSync(outRoot, { recursive: true, force: true });
	mkdirSync(outRoot, { recursive: true });
	const stub = await startRecordingStub(path.join(outRoot, "raw-requests.jsonl"));
	console.error(`[m10b] recording stub on 127.0.0.1:${stub.port}`);
	const builds = [];
	try {
		for (const c of CASES) {
			const caseDir = path.join(outRoot, c.name);
			const { agentDir } = writeAgentDir(caseDir, c, stub.port);
			const before = readRecordings(path.join(outRoot, "raw-requests.jsonl")).length;
			const { finalText, usage } = await runHost(caseDir, agentDir, c.retentionEnv);
			const recs = readRecordings(path.join(outRoot, "raw-requests.jsonl")).slice(before)
				.filter((r) => r.method === "POST" && (r.url.endsWith("/chat/completions") || r.url.endsWith("/messages")));
			if (recs.length === 0) throw new Error(`[${c.name}] no provider request recorded`);
			const cache = cacheFields(recs[recs.length - 1]);
			builds.push({
				type: "prompt-build",
				case: c.name,
				provider_family: c.family,
				pi_cache_retention_env: c.retentionEnv ?? "(unset→upstream short)",
				compat_override: c.compat,
				requests_recorded: recs.length,
				cache,
				usage: usage ? { input: usage.input, output: usage.output, cacheRead: usage.cacheRead, cacheWrite: usage.cacheWrite } : null,
				final_text: finalText,
			});
			console.error(`[m10b] case ${c.name}: ${recs.length} request(s), final=${JSON.stringify(finalText)}`);
		}
	} finally {
		await stub.close();
	}

	mkdirSync(path.dirname(tracesPath), { recursive: true });
	writeFileSync(tracesPath, builds.map((b) => JSON.stringify(b)).join("\n") + "\n");
	console.error(`[m10b] prompt-builds -> ${tracesPath}`);

	const byCase = Object.fromEntries(builds.map((b) => [b.case, b]));
	let pass = true;
	// Every case must have produced the stub's canned answer (host ↔ provider OK).
	for (const b of builds) pass = assertCase(b.case, b.final_text.includes("CACHE-PROBE-OK"), "assistant turn completed") && pass;
	// openai-completions (GLM-shim shape)
	pass = assertCase("openai-default", byCase["openai-default"].cache.prompt_cache_key === null && byCase["openai-default"].cache.prompt_cache_retention === null, "no cache fields at default retention") && pass;
	pass = assertCase("openai-long", byCase["openai-long"].cache.prompt_cache_retention === "24h" && typeof byCase["openai-long"].cache.prompt_cache_key === "string" && byCase["openai-long"].cache.prompt_cache_key.length <= 64, `PI_CACHE_RETENTION=long adds prompt_cache_key+retention:24h (upstream raw behavior on generic providers; key=${JSON.stringify(byCase["openai-long"].cache.prompt_cache_key)})`) && pass;
	pass = assertCase("openai-long-compat", byCase["openai-long-compat"].cache.prompt_cache_key === null && byCase["openai-long-compat"].cache.prompt_cache_retention === null, "compat.supportsLongCacheRetention=false keeps long-retention payload byte-clean (upstream #7676 pattern)") && pass;
	// anthropic-messages (Claude OAuth shape at cutover)
	const ad = byCase["anthropic-default"].cache;
	pass = assertCase("anthropic-default", ad.system_cc.count === 1 && ad.tool_cc.count === 1 && ad.last_user_cc.count === 1 && [...ad.system_cc.ttls, ...ad.tool_cc.ttls, ...ad.last_user_cc.ttls].every((t) => t === "(none)"), `3 ephemeral breakpoints (system=${ad.system_cc.count} tool=${ad.tool_cc.count} user=${ad.last_user_cc.count}), no ttl`) && pass;
	const al = byCase["anthropic-long"].cache;
	pass = assertCase("anthropic-long", al.system_cc.count === 1 && al.tool_cc.count === 1 && al.last_user_cc.count === 1 && [...al.system_cc.ttls, ...al.tool_cc.ttls, ...al.last_user_cc.ttls].every((t) => t === "1h"), "PI_CACHE_RETENTION=long adds ttl:1h to all 3 breakpoints (5m→1h write pricing)") && pass;

	if (!pass) {
		console.error("=== M10b PAYLOAD EVIDENCE: FAIL ===");
		process.exit(1);
	}
	console.error("=== M10b PAYLOAD EVIDENCE: PASS ===");
}

main().catch((e) => {
	console.error(e);
	process.exit(1);
});
