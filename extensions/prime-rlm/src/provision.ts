// Simplified port of prime-agent's ensureKernelPython (core/kernel/bootstrap.ts).
// M1: no bootstrap lock, no python-skill sync, no state-snapshot requirement.
// Resolves a Python with ipykernel + the prime-rlm-runtime package:
//   1. PRIME_RLM_KERNEL_PYTHON env (explicit override, must already have both)
//   2. PRIME_RLM_KERNEL_VENV env or <agentDir>/prime-rlm/kernel-venv (auto-provisioned via uv)
import { execFile } from "node:child_process";
import { createHash } from "node:crypto";
import { existsSync } from "node:fs";
import { mkdir, readFile, writeFile } from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { promisify } from "node:util";

const run = promisify(execFile);

const PYTHON_VERSION = "3.12";
const IPYKERNEL_REQUIREMENT = "ipykernel>=6.30.0";
/** Extra runtime deps: dill powers kernel-state snapshots (state-snapshot.ts).
 * M4: httpx + Pillow back the bundled websearch/attach-image skills. */
// PA parity (DEFAULT_RLM_EXTRA_PACKAGES): the same pre-installed set, so the
// prompt's package labels tell the truth.
const EXTRA_REQUIREMENTS = [
	"nest_asyncio",
	"dill",
	"httpx",
	"Pillow",
	"requests",
	"pyyaml",
	"tomli",
	"python-dotenv",
	"pandas",
	"numpy",
	"scipy",
	"beautifulsoup4",
	"lxml",
	"pydantic",
	"tyro",
] as const;
/** Prompt labels for the pre-installed packages (PA DEFAULT_RLM_EXTRA_IMPORT_LABELS). */
export const PREINSTALLED_PACKAGE_LABELS = [
	"requests",
	"httpx",
	"yaml (PyYAML)",
	"tomli",
	"dotenv (python-dotenv)",
	"pandas",
	"numpy",
	"scipy",
	"bs4 (Beautiful Soup)",
	"lxml",
	"pydantic",
	"tyro",
] as const;
const BOOTSTRAP_VERSION_FILE = ".prime-rlm-bootstrap.json";

const moduleDir = path.dirname(fileURLToPath(import.meta.url));
/** The runtime python package shipped inside this extension (python/prime-rlm-runtime). */
export const RUNTIME_SOURCE_DIR = path.resolve(moduleDir, "..", "python", "prime-rlm-runtime");

function errorMessage(error: unknown): string {
	return error instanceof Error ? error.message : String(error);
}

async function isExecutable(file: string): Promise<boolean> {
	try {
		await run(file, ["--version"]);
		return true;
	} catch {
		return false;
	}
}

async function findExecutable(name: string): Promise<string | null> {
	const pathValue = process.env.PATH;
	if (!pathValue) return null;
	const candidates = process.platform === "win32" ? [name, `${name}.exe`] : [name];
	for (const dir of pathValue.split(path.delimiter)) {
		if (!dir) continue;
		for (const candidate of candidates) {
			const fullPath = path.join(dir, candidate);
			if (await isExecutable(fullPath)) return fullPath;
		}
	}
	return null;
}

async function ensureUv(): Promise<string> {
	const fromPath = await findExecutable("uv");
	if (fromPath) return fromPath;
	const localUv = path.join(os.homedir(), ".nix-profile", "bin", "uv");
	if (await isExecutable(localUv)) return localUv;
	const dotLocal = path.join(os.homedir(), ".local", "bin", "uv");
	if (await isExecutable(dotLocal)) return dotLocal;
	throw new Error("uv is required to set up the Python kernel (install uv or set PRIME_RLM_KERNEL_PYTHON)");
}

/** Hash the runtime package so edits invalidate the venv stamp. */
async function runtimeIdentity(): Promise<string> {
	const { readdir } = await import("node:fs/promises");
	const hash = createHash("sha256");
	const files: string[] = [path.join(RUNTIME_SOURCE_DIR, "pyproject.toml")];
	async function collect(dir: string): Promise<void> {
		for (const entry of await readdir(dir, { withFileTypes: true })) {
			const full = path.join(dir, entry.name);
			if (entry.isDirectory()) await collect(full);
			else if (entry.isFile() && entry.name.endsWith(".py")) files.push(full);
		}
	}
	await collect(path.join(RUNTIME_SOURCE_DIR, "src", "prime_rlm_runtime"));
	files.sort();
	for (const file of files) {
		hash.update(path.relative(RUNTIME_SOURCE_DIR, file));
		hash.update("\0");
		hash.update(await readFile(file));
		hash.update("\0");
	}
	return `sha256:${hash.digest("hex")}`;
}

async function stampCurrent(venv: string, identity: string): Promise<boolean> {
	try {
		const raw = await readFile(path.join(venv, BOOTSTRAP_VERSION_FILE), "utf8");
		const parsed = JSON.parse(raw);
		return (
			parsed.runtime === identity &&
			parsed.ipykernel === IPYKERNEL_REQUIREMENT &&
			JSON.stringify(parsed.extras ?? []) === JSON.stringify([...EXTRA_REQUIREMENTS])
		);
	} catch {
		return false;
	}
}

async function pythonHasRuntime(python: string): Promise<boolean> {
	try {
		await run(python, ["-c", "import ipykernel, prime_rlm_runtime, dill"]);
		return true;
	} catch {
		return false;
	}
}

export interface EnsureKernelPythonOptions {
	agentDir: string;
	onProgress?: (message: string) => void;
}

/**
 * Resolve a Python interpreter with ipykernel + prime-rlm-runtime.
 * Provisions a uv venv on first use; subsequent calls are stamp-checked.
 */
export async function ensureKernelPython(options: EnsureKernelPythonOptions): Promise<string> {
	const explicit = process.env.PRIME_RLM_KERNEL_PYTHON;
	if (explicit) {
		return explicit;
	}

	const venv = process.env.PRIME_RLM_KERNEL_VENV ?? path.join(options.agentDir, "prime-rlm", "kernel-venv");
	const python = path.join(venv, "bin", "python");
	const identity = await runtimeIdentity();

	if (existsSync(python) && (await stampCurrent(venv, identity)) && (await pythonHasRuntime(python))) {
		return python;
	}

	options.onProgress?.("› setting up Python kernel environment (one-time)…");
	const uv = await ensureUv();
	if (!existsSync(python)) {
		await mkdir(venv, { recursive: true });
		await run(uv, ["python", "install", PYTHON_VERSION]);
		await run(uv, ["venv", venv, "--python", PYTHON_VERSION, "--seed"]);
	}
	await run(uv, [
		"pip",
		"install",
		"--python",
		python,
		IPYKERNEL_REQUIREMENT,
		...EXTRA_REQUIREMENTS,
		RUNTIME_SOURCE_DIR,
	]);
	await writeFile(
		path.join(venv, BOOTSTRAP_VERSION_FILE),
		`${JSON.stringify({ runtime: identity, ipykernel: IPYKERNEL_REQUIREMENT, extras: [...EXTRA_REQUIREMENTS] })}\n`,
		"utf8",
	);
	if (!(await pythonHasRuntime(python))) {
		throw new Error(`provisioned kernel python at ${python} is missing prime_rlm_runtime`);
	}
	return python;
}
