import { spawnSync } from "node:child_process";
import { dirname, resolve } from "node:path";
import process from "node:process";
import { fileURLToPath } from "node:url";

const scriptDir = dirname(fileURLToPath(import.meta.url));
const repoRoot = resolve(scriptDir, "../..");
const tsEntry = resolve(scriptDir, "./orchestrator-replay.ts");

const child = spawnSync(process.execPath, ["--import", "tsx", tsEntry, ...process.argv.slice(2)], {
	cwd: repoRoot,
	stdio: "inherit",
});

if (child.error) {
	console.error(child.error instanceof Error ? child.error.message : String(child.error));
	process.exit(1);
}

process.exit(child.status ?? 1);
