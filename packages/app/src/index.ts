#!/usr/bin/env node

import { spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";

function hasNodeFlag(flag: string) {
	if (process.execArgv.includes(flag)) {
		return true;
	}
	const nodeOptions = process.env.NODE_OPTIONS?.split(/\s+/) ?? [];
	return nodeOptions.includes(flag);
}

// Preserve workspace symlinks so packages resolve through a single dependency graph.
if (!hasNodeFlag("--preserve-symlinks")) {
	const mainPath = fileURLToPath(new URL("./main.js", import.meta.url));
	const child = spawnSync(
		process.execPath,
		["--preserve-symlinks", "--preserve-symlinks-main", ...process.execArgv, mainPath, ...process.argv.slice(2)],
		{
			stdio: "inherit",
			env: process.env,
		},
	);
	if (child.error) {
		throw child.error;
	}
	if (child.signal) {
		process.kill(process.pid, child.signal);
	}
	process.exit(child.status ?? 1);
}

await import("./main.js");
