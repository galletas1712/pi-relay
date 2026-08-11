// Bridge lifecycle helpers for B scenarios: start/stop/kill the bridge and
// session hosts, read the auth token, talk to the test PG via docker exec.
import { spawn, execFileSync } from "node:child_process";
import { readFileSync, existsSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

export const BRIDGE_DIR = join(dirname(fileURLToPath(import.meta.url)), "..");
export const DEMO_DIR = join(BRIDGE_DIR, "..", "..", ".pi", "m1-demo");
export const TRACES_DIR = join(DEMO_DIR, "traces");
export const BRIDGE_PORT = process.env.BRIDGE_PORT ?? "8730";
export const BRIDGE_URL = `ws://127.0.0.1:${BRIDGE_PORT}`;
export const ORIGIN = "http://localhost:3000";

export function authToken() {
	return readFileSync(join(DEMO_DIR, ".bridge-auth-token"), "utf8").trim();
}

export function bridgePid() {
	const f = join(BRIDGE_DIR, "data", "bridge.pid");
	return existsSync(f) ? Number(readFileSync(f, "utf8").trim()) : null;
}

// Boot respawns every non-closed session host sequentially (M11a: the test
// rig has accumulated 60+ sessions across milestone runs), so healthy-latency
// scales with session count; 45 s was sized for a young rig.
async function waitHealthy(timeoutMs = 180000) {
	const t0 = Date.now();
	while (Date.now() - t0 < timeoutMs) {
		try {
			const res = await fetch(`http://127.0.0.1:${BRIDGE_PORT}/healthz`);
			if (res.ok) return true;
		} catch {}
		await new Promise((r) => setTimeout(r, 250));
	}
	throw new Error("bridge did not become healthy");
}

export async function startBridge(extraEnv = {}) {
	const { openSync } = await import("node:fs");
	const fd = openSync(join(BRIDGE_DIR, "data", "bridge-test.out"), "a");
	const child = spawn("bash", [join(BRIDGE_DIR, "run-bridge.sh")], {
		env: { ...process.env, ...extraEnv },
		stdio: ["ignore", fd, fd],
		detached: true,
	});
	child.unref();
	await waitHealthy();
	return child.pid;
}

export async function stopBridge(graceMs = 8000) {
	const pid = bridgePid();
	if (!pid) return;
	try { process.kill(pid, "SIGTERM"); } catch { return; }
	await waitDead(pid, graceMs);
}

export async function killBridge() {
	const pid = bridgePid();
	if (!pid) return;
	try { process.kill(pid, "SIGKILL"); } catch {}
	await waitDead(pid, 8000);
}

export async function waitDead(pid, timeoutMs = 8000) {
	const t0 = Date.now();
	while (Date.now() - t0 < timeoutMs) {
		try { process.kill(pid, 0); } catch { return true; }
		await new Promise((r) => setTimeout(r, 150));
	}
	return false; // still alive
}

export function pidAlive(pid) {
	try { process.kill(pid, 0); return true; } catch { return false; }
}

export function psql(sql) {
	return execFileSync(
		"docker",
		["exec", "pi-relay-bridge-test-pg", "psql", "-U", "postgres", "-d", "pi_relay_bridge", "-At", "-c", sql],
		{ encoding: "utf8" },
	).trim();
}
