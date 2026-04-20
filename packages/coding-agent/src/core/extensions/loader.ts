/**
 * Extension loader - loads TypeScript extension modules using jiti.
 *
 * Uses @mariozechner/jiti fork with virtualModules support for compiled Bun binaries.
 */

import * as fs from "node:fs";
import { createRequire } from "node:module";
import * as os from "node:os";
import * as path from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import { createJiti } from "@mariozechner/jiti";
import * as _bundledPiAgentCore from "@pi-relay/agent-core";
import * as _bundledPiAi from "@pi-relay/ai";
import * as _bundledPiAiOauth from "@pi-relay/ai/oauth";
import * as _bundledPiToolKit from "@pi-relay/tool-kit";
import * as _bundledPiToolKitRender from "@pi-relay/tool-kit/render";
import type { KeyId } from "@pi-relay/tui";
import * as _bundledPiTui from "@pi-relay/tui";
// Static imports of packages that extensions may use.
// These MUST be static so Bun bundles them into the compiled binary.
// The virtualModules option then makes them available to extensions.
import * as _bundledTypebox from "@sinclair/typebox";
import { getAgentDir, isBunBinary } from "../../config.js";
// NOTE: This import works because loader.ts exports are NOT re-exported from index.ts,
// avoiding a circular dependency. Extensions can import from @pi-relay/coding-agent.
import * as _bundledPiCodingAgent from "../../index.js";
import { createEventBus, type EventBus } from "../event-bus.js";
import type { ExecOptions } from "../exec.js";
import { execCommand } from "../exec.js";
import { createSyntheticSourceInfo } from "../source-info.js";
import { createToolHost } from "../tool-packages/host.js";
import { defaultToolInterfaceRegistry } from "../tool-packages/interfaces.js";
import {
	configureToolsInRegistry,
	registerToolProviderInExtension,
} from "../tool-packages/register.js";
import { ToolRegistry } from "../tool-packages/tools.js";
import type {
	Extension,
	ExtensionAPI,
	ExtensionFactory,
	ExtensionRuntime,
	LoadExtensionsResult,
	MessageRenderer,
	ProviderConfig,
	RegisteredCommand,
	ToolDefinition,
} from "./types.js";

/** Modules available to extensions via virtualModules (for compiled Bun binary) */
const VIRTUAL_MODULES: Record<string, unknown> = {
	"@sinclair/typebox": _bundledTypebox,
	"@pi-relay/agent-core": _bundledPiAgentCore,
	"@pi-relay/tui": _bundledPiTui,
	"@pi-relay/ai": _bundledPiAi,
	"@pi-relay/ai/oauth": _bundledPiAiOauth,
	"@pi-relay/tool-kit": _bundledPiToolKit,
	"@pi-relay/tool-kit/render": _bundledPiToolKitRender,
	"@pi-relay/coding-agent": _bundledPiCodingAgent,
};

const require = createRequire(import.meta.url);

/**
 * Get aliases for jiti (used in Node.js/development mode).
 * In Bun binary mode, virtualModules is used instead.
 */
let _aliases: Record<string, string> | null = null;
function getAliases(): Record<string, string> {
	if (_aliases) return _aliases;

	const __dirname = path.dirname(fileURLToPath(import.meta.url));
	const packageIndex = path.resolve(__dirname, "../..", "index.js");

	const typeboxEntry = require.resolve("@sinclair/typebox");
	const typeboxRoot = typeboxEntry.replace(/[\\/]build[\\/]cjs[\\/]index\.js$/, "");

	const packagesRoot = path.resolve(__dirname, "../../../../");
	const resolveWorkspaceOrImport = (workspaceRelativePath: string, specifier: string): string => {
		const workspacePath = path.join(packagesRoot, workspaceRelativePath);
		if (fs.existsSync(workspacePath)) {
			return workspacePath;
		}
		return fileURLToPath(import.meta.resolve(specifier));
	};

	_aliases = {
		"@pi-relay/coding-agent": packageIndex,
		"@pi-relay/agent-core": resolveWorkspaceOrImport("agent-core/dist/index.js", "@pi-relay/agent-core"),
		"@pi-relay/tui": resolveWorkspaceOrImport("tui/dist/index.js", "@pi-relay/tui"),
		"@pi-relay/ai": resolveWorkspaceOrImport("ai/dist/index.js", "@pi-relay/ai"),
		"@pi-relay/ai/oauth": resolveWorkspaceOrImport("ai/dist/oauth.js", "@pi-relay/ai/oauth"),
		"@pi-relay/tool-kit": resolveWorkspaceOrImport("tool-kit/dist/index.js", "@pi-relay/tool-kit"),
		"@pi-relay/tool-kit/render": resolveWorkspaceOrImport("tool-kit/dist/render.js", "@pi-relay/tool-kit/render"),
		"@sinclair/typebox": typeboxRoot,
	};

	return _aliases;
}

const UNICODE_SPACES = /[\u00A0\u2000-\u200A\u202F\u205F\u3000]/g;

function normalizeUnicodeSpaces(str: string): string {
	return str.replace(UNICODE_SPACES, " ");
}

function expandPath(p: string): string {
	const normalized = normalizeUnicodeSpaces(p);
	if (normalized.startsWith("~/")) {
		return path.join(os.homedir(), normalized.slice(2));
	}
	if (normalized.startsWith("~")) {
		return path.join(os.homedir(), normalized.slice(1));
	}
	return normalized;
}

function resolvePath(extPath: string, cwd: string): string {
	const expanded = expandPath(extPath);
	if (path.isAbsolute(expanded)) {
		return expanded;
	}
	return path.resolve(cwd, expanded);
}

/**
 * Normalized form for an entry from `settings.extensions` /
 * `piConfig.extensions` / the `-e` CLI flag. Legacy string entries are
 * either file paths or bare package names; the object forms are explicit.
 */
export type ExtensionEntry = string | { path: string } | { package: string };

interface ResolvedEntry {
	kind: "file" | "package";
	/** Absolute filesystem path to the module's entry file (the thing jiti imports). */
	resolvedPath: string;
	/** Display / sourceInfo form of the original entry (e.g. `"@pi-relay/extensions"` for package entries). */
	displayName: string;
	/** Package name when `kind === "package"`. */
	packageName?: string;
}

function looksLikePackageName(value: string): boolean {
	const trimmed = value.trim();
	if (!trimmed) return false;
	if (trimmed.startsWith(".") || trimmed.startsWith("/") || trimmed.startsWith("~")) return false;
	if (/^[A-Za-z]:[\\/]/.test(trimmed)) return false;
	if (/^[A-Za-z][A-Za-z0-9+.-]*:/.test(trimmed)) return false;
	if (trimmed.startsWith("@")) {
		const parts = trimmed.split("/");
		return parts.length === 2 && parts[0].length >= 2 && parts[1].length >= 1;
	}
	return !trimmed.includes("/");
}

/**
 * Resolve a single `ExtensionEntry` into a file path (for file entries) or
 * a Node-resolved entry module path (for package entries).
 *
 * Errors (package not found, unreadable path) are returned via the
 * `ResolvedEntry | { error }` discriminator instead of thrown so the caller
 * can surface them as per-extension diagnostics without crashing the
 * session.
 */
export function resolveExtensionEntry(
	entry: ExtensionEntry,
	cwd: string,
): ResolvedEntry | { error: string; displayName: string } {
	let kind: "file" | "package";
	let value: string;
	if (typeof entry === "string") {
		value = entry;
		kind = looksLikePackageName(value) ? "package" : "file";
	} else if ("package" in entry) {
		value = entry.package;
		kind = "package";
	} else {
		value = entry.path;
		kind = "file";
	}

	if (kind === "file") {
		const resolved = resolvePath(value, cwd);
		return { kind, resolvedPath: resolved, displayName: value };
	}

	// Package resolution. Try each of these in order:
	//   1. `import.meta.resolve` anchored at `<cwd>/package.json` — honors ESM
	//      `exports.import` conditions, symlinked workspaces, and scoped names.
	//   2. `createRequire(...).resolve(...)` — fallback for CommonJS packages
	//      or when import.meta.resolve isn't exposed for some reason.
	//   3. Manual probe of `node_modules/<name>/<main-from-package.json>` —
	//      covers the workspace-symlink case when Node's resolver can't see
	//      the package because it has only an `exports` field (no `main`).
	const anchor = pathToFileURL(path.join(cwd, "package.json")).href;
	try {
		const resolvedUrl = import.meta.resolve(value, anchor);
		const resolvedPath = fileURLToPath(resolvedUrl);
		return { kind: "package", resolvedPath, displayName: value, packageName: value };
	} catch {
		// Fall through to createRequire.
	}

	let createRequireError: string | undefined;
	try {
		const req = createRequire(anchor);
		const resolvedPath = req.resolve(value);
		return { kind: "package", resolvedPath, displayName: value, packageName: value };
	} catch (err) {
		createRequireError = err instanceof Error ? err.message : String(err);
	}

	// Manual probe as a last resort. Walk up from cwd looking for
	// `node_modules/<value>/package.json`, then resolve its `main` /
	// `exports["."].import` entry.
	const probed = probeNodeModulesEntry(value, cwd);
	if (probed) {
		return { kind: "package", resolvedPath: probed, displayName: value, packageName: value };
	}
	return {
		error: `Could not resolve package "${value}" from ${cwd}: ${createRequireError ?? "not found in node_modules"}`,
		displayName: value,
	};
}

/**
 * Walk up the directory tree from `startDir` looking for
 * `node_modules/<pkg>/package.json`, then pick the entry file (prefers
 * ESM exports `.` + `import`, falls back to `main`, falls back to
 * `index.js`).
 */
function probeNodeModulesEntry(pkgName: string, startDir: string): string | undefined {
	let dir = startDir;
	while (true) {
		const pkgDir = path.join(dir, "node_modules", pkgName);
		const pkgJson = path.join(pkgDir, "package.json");
		if (fs.existsSync(pkgJson)) {
			try {
				const manifest = JSON.parse(fs.readFileSync(pkgJson, "utf-8"));
				const exportsField = manifest.exports;
				if (exportsField && typeof exportsField === "object") {
					const dotExport = (exportsField as Record<string, unknown>)["."];
					if (typeof dotExport === "string") {
						return path.resolve(pkgDir, dotExport);
					}
					if (dotExport && typeof dotExport === "object") {
						const dotObj = dotExport as Record<string, unknown>;
						const candidate =
							(typeof dotObj.import === "string" && (dotObj.import as string)) ||
							(typeof dotObj.default === "string" && (dotObj.default as string)) ||
							(typeof dotObj.require === "string" && (dotObj.require as string)) ||
							undefined;
						if (candidate) return path.resolve(pkgDir, candidate);
					}
				}
				if (typeof manifest.main === "string") {
					return path.resolve(pkgDir, manifest.main);
				}
				const indexJs = path.join(pkgDir, "index.js");
				if (fs.existsSync(indexJs)) return indexJs;
			} catch {
				return undefined;
			}
		}
		const parent = path.dirname(dir);
		if (parent === dir) return undefined;
		dir = parent;
	}
}

type HandlerFn = (...args: unknown[]) => Promise<unknown>;

/**
 * Create a runtime with throwing stubs for action methods.
 * Runner.bindCore() replaces these with real implementations.
 */
export function createExtensionRuntime(): ExtensionRuntime {
	const notInitialized = () => {
		throw new Error("Extension runtime not initialized. Action methods cannot be called during extension loading.");
	};

	const toolRegistry = new ToolRegistry({
		interfaces: defaultToolInterfaceRegistry,
		hostFactory: (ctx) => createToolHost(ctx),
	});

	const runtime: ExtensionRuntime = {
		sendMessage: notInitialized,
		sendUserMessage: notInitialized,
		appendEntry: notInitialized,
		setSessionName: notInitialized,
		getSessionName: notInitialized,
		setLabel: notInitialized,
		getActiveTools: notInitialized,
		getAllTools: notInitialized,
		setActiveTools: notInitialized,
		// registerTool() is valid during extension load; refresh is only needed post-bind.
		refreshTools: () => {},
		getCommands: notInitialized,
		setModel: () => Promise.reject(new Error("Extension runtime not initialized")),
		getThinkingLevel: notInitialized,
		setThinkingLevel: notInitialized,
		flagValues: new Map(),
		pendingProviderRegistrations: [],
		toolRegistry,
		// Pre-bind: queue registrations so bindCore() can flush them once the
		// model registry is available. bindCore() replaces both with direct calls.
		registerProvider: (name, config, extensionPath = "<unknown>") => {
			runtime.pendingProviderRegistrations.push({ name, config, extensionPath });
		},
		unregisterProvider: (name) => {
			runtime.pendingProviderRegistrations = runtime.pendingProviderRegistrations.filter((r) => r.name !== name);
		},
	};

	return runtime;
}

/**
 * Create the ExtensionAPI for an extension.
 * Registration methods write to the extension object.
 * Action methods delegate to the shared runtime.
 */
function createExtensionAPI(
	extension: Extension,
	runtime: ExtensionRuntime,
	cwd: string,
	eventBus: EventBus,
): ExtensionAPI {
	const api = {
		// Registration methods - write to extension
		on(event: string, handler: HandlerFn): void {
			const list = extension.handlers.get(event) ?? [];
			list.push(handler);
			extension.handlers.set(event, list);
		},

		registerTool(tool: ToolDefinition): void {
			extension.tools.set(tool.name, {
				definition: tool,
				sourceInfo: extension.sourceInfo,
			});
			runtime.refreshTools();
		},

		registerToolProvider(provider): void {
			registerToolProviderInExtension(
				extension,
				runtime.toolRegistry,
				// Author-side generics are erased at the registry boundary. The
				// provider's own execute closure keeps typed config/secrets via
				// closure capture at author side.
				provider as unknown as import("@pi-relay/tool-kit").ToolProvider,
				(message) => console.warn(message),
			);
			runtime.refreshTools();
		},

		configureTools(config): void {
			configureToolsInRegistry(runtime.toolRegistry, config);
			runtime.refreshTools();
		},

		registerCommand(name: string, options: Omit<RegisteredCommand, "name" | "sourceInfo">): void {
			extension.commands.set(name, {
				name,
				sourceInfo: extension.sourceInfo,
				...options,
			});
		},

		registerShortcut(
			shortcut: KeyId,
			options: {
				description?: string;
				handler: (ctx: import("./types.js").ExtensionContext) => Promise<void> | void;
			},
		): void {
			extension.shortcuts.set(shortcut, { shortcut, extensionPath: extension.path, ...options });
		},

		registerFlag(
			name: string,
			options: { description?: string; type: "boolean" | "string"; default?: boolean | string },
		): void {
			extension.flags.set(name, { name, extensionPath: extension.path, ...options });
			if (options.default !== undefined && !runtime.flagValues.has(name)) {
				runtime.flagValues.set(name, options.default);
			}
		},

		registerMessageRenderer<T>(customType: string, renderer: MessageRenderer<T>): void {
			extension.messageRenderers.set(customType, renderer as MessageRenderer);
		},

		// Flag access - checks extension registered it, reads from runtime
		getFlag(name: string): boolean | string | undefined {
			if (!extension.flags.has(name)) return undefined;
			return runtime.flagValues.get(name);
		},

		// Action methods - delegate to shared runtime
		sendMessage(message, options): void {
			runtime.sendMessage(message, options);
		},

		sendUserMessage(content, options): void {
			runtime.sendUserMessage(content, options);
		},

		appendEntry(customType: string, data?: unknown): void {
			runtime.appendEntry(customType, data);
		},

		setSessionName(name: string): void {
			runtime.setSessionName(name);
		},

		getSessionName(): string | undefined {
			return runtime.getSessionName();
		},

		setLabel(entryId: string, label: string | undefined): void {
			runtime.setLabel(entryId, label);
		},

		exec(command: string, args: string[], options?: ExecOptions) {
			return execCommand(command, args, options?.cwd ?? cwd, options);
		},

		getActiveTools(): string[] {
			return runtime.getActiveTools();
		},

		getAllTools() {
			return runtime.getAllTools();
		},

		setActiveTools(toolNames: string[]): void {
			runtime.setActiveTools(toolNames);
		},

		getCommands() {
			return runtime.getCommands();
		},

		setModel(model) {
			return runtime.setModel(model);
		},

		getThinkingLevel() {
			return runtime.getThinkingLevel();
		},

		setThinkingLevel(level) {
			runtime.setThinkingLevel(level);
		},

		registerProvider(name: string, config: ProviderConfig) {
			runtime.registerProvider(name, config, extension.path);
		},

		unregisterProvider(name: string) {
			runtime.unregisterProvider(name, extension.path);
		},

		events: eventBus,
	} as ExtensionAPI;

	return api;
}

async function loadExtensionModule(extensionPath: string) {
	const jiti = createJiti(import.meta.url, {
		moduleCache: false,
		// In Bun binary: use virtualModules for bundled packages (no filesystem resolution)
		// Also disable tryNative so jiti handles ALL imports (not just the entry point)
		// In Node.js/dev: use aliases to resolve to node_modules paths
		...(isBunBinary ? { virtualModules: VIRTUAL_MODULES, tryNative: false } : { alias: getAliases() }),
	});

	const module = await jiti.import(extensionPath, { default: true });
	const factory = module as ExtensionFactory;
	return typeof factory !== "function" ? undefined : factory;
}

/**
 * Create an Extension object with empty collections.
 */
function createExtension(extensionPath: string, resolvedPath: string): Extension {
	const source =
		extensionPath.startsWith("<") && extensionPath.endsWith(">")
			? extensionPath.slice(1, -1).split(":")[0] || "temporary"
			: "local";
	const baseDir = extensionPath.startsWith("<") ? undefined : path.dirname(resolvedPath);

	return {
		path: extensionPath,
		resolvedPath,
		sourceInfo: createSyntheticSourceInfo(extensionPath, { source, baseDir }),
		handlers: new Map(),
		tools: new Map(),
		messageRenderers: new Map(),
		commands: new Map(),
		flags: new Map(),
		shortcuts: new Map(),
		toolProviders: new Map(),
	};
}

async function loadExtension(
	entry: ExtensionEntry,
	cwd: string,
	eventBus: EventBus,
	runtime: ExtensionRuntime,
): Promise<{ extension: Extension | null; error: string | null; displayName: string }> {
	const resolved = resolveExtensionEntry(entry, cwd);
	if ("error" in resolved) {
		return { extension: null, error: resolved.error, displayName: resolved.displayName };
	}

	try {
		const factory = await loadExtensionModule(resolved.resolvedPath);
		if (!factory) {
			return {
				extension: null,
				error: `Extension does not export a valid factory function: ${resolved.displayName}`,
				displayName: resolved.displayName,
			};
		}

		const extension = createExtension(resolved.displayName, resolved.resolvedPath);
		if (resolved.kind === "package") {
			// Mark this extension as package-sourced so diagnostics / `/tools`
			// can distinguish a package entry from a raw file path.
			extension.sourceInfo = {
				...extension.sourceInfo,
				source: "package",
			};
		}
		const api = createExtensionAPI(extension, runtime, cwd, eventBus);
		await factory(api);

		return { extension, error: null, displayName: resolved.displayName };
	} catch (err) {
		const message = err instanceof Error ? err.message : String(err);
		return { extension: null, error: `Failed to load extension: ${message}`, displayName: resolved.displayName };
	}
}

/**
 * Create an Extension from an inline factory function.
 */
export async function loadExtensionFromFactory(
	factory: ExtensionFactory,
	cwd: string,
	eventBus: EventBus,
	runtime: ExtensionRuntime,
	extensionPath = "<inline>",
): Promise<Extension> {
	const extension = createExtension(extensionPath, extensionPath);
	const api = createExtensionAPI(extension, runtime, cwd, eventBus);
	await factory(api);
	return extension;
}

/**
 * Load extensions from a list of entries. Accepts:
 *   - string (legacy): file path OR bare package name (distinguished
 *     heuristically).
 *   - `{ path: "..." }`: always a file path.
 *   - `{ package: "..." }`: always a package name, resolved via Node module
 *     resolution anchored at `cwd`.
 */
export async function loadExtensions(
	entries: ExtensionEntry[],
	cwd: string,
	eventBus?: EventBus,
): Promise<LoadExtensionsResult> {
	const extensions: Extension[] = [];
	const errors: Array<{ path: string; error: string }> = [];
	const resolvedEventBus = eventBus ?? createEventBus();
	const runtime = createExtensionRuntime();

	for (const entry of entries) {
		const { extension, error, displayName } = await loadExtension(entry, cwd, resolvedEventBus, runtime);

		if (error) {
			errors.push({ path: displayName, error });
			continue;
		}

		if (extension) {
			extensions.push(extension);
		}
	}

	return {
		extensions,
		errors,
		runtime,
	};
}

interface PiManifest {
	extensions?: string[];
	themes?: string[];
	skills?: string[];
	prompts?: string[];
}

function readPiManifest(packageJsonPath: string): PiManifest | null {
	try {
		const content = fs.readFileSync(packageJsonPath, "utf-8");
		const pkg = JSON.parse(content);
		if (pkg.pi && typeof pkg.pi === "object") {
			return pkg.pi as PiManifest;
		}
		return null;
	} catch {
		return null;
	}
}

function isExtensionFile(name: string): boolean {
	return name.endsWith(".ts") || name.endsWith(".js");
}

/**
 * Resolve extension entry points from a directory.
 *
 * Checks for:
 * 1. package.json with "pi.extensions" field -> returns declared paths
 * 2. index.ts or index.js -> returns the index file
 *
 * Returns resolved paths or null if no entry points found.
 */
function resolveExtensionEntries(dir: string): string[] | null {
	// Check for package.json with "pi" field first
	const packageJsonPath = path.join(dir, "package.json");
	if (fs.existsSync(packageJsonPath)) {
		const manifest = readPiManifest(packageJsonPath);
		if (manifest?.extensions?.length) {
			const entries: string[] = [];
			for (const extPath of manifest.extensions) {
				const resolvedExtPath = path.resolve(dir, extPath);
				if (fs.existsSync(resolvedExtPath)) {
					entries.push(resolvedExtPath);
				}
			}
			if (entries.length > 0) {
				return entries;
			}
		}
	}

	// Check for index.ts or index.js
	const indexTs = path.join(dir, "index.ts");
	const indexJs = path.join(dir, "index.js");
	if (fs.existsSync(indexTs)) {
		return [indexTs];
	}
	if (fs.existsSync(indexJs)) {
		return [indexJs];
	}

	return null;
}

/**
 * Discover extensions in a directory.
 *
 * Discovery rules:
 * 1. Direct files: `extensions/*.ts` or `*.js` → load
 * 2. Subdirectory with index: `extensions/* /index.ts` or `index.js` → load
 * 3. Subdirectory with package.json: `extensions/* /package.json` with "pi" field → load what it declares
 *
 * No recursion beyond one level. Complex packages must use package.json manifest.
 */
function discoverExtensionsInDir(dir: string): string[] {
	if (!fs.existsSync(dir)) {
		return [];
	}

	const discovered: string[] = [];

	try {
		const entries = fs.readdirSync(dir, { withFileTypes: true });

		for (const entry of entries) {
			const entryPath = path.join(dir, entry.name);

			// 1. Direct files: *.ts or *.js
			if ((entry.isFile() || entry.isSymbolicLink()) && isExtensionFile(entry.name)) {
				discovered.push(entryPath);
				continue;
			}

			// 2 & 3. Subdirectories
			if (entry.isDirectory() || entry.isSymbolicLink()) {
				const entries = resolveExtensionEntries(entryPath);
				if (entries) {
					discovered.push(...entries);
				}
			}
		}
	} catch {
		return [];
	}

	return discovered;
}

/**
 * Discover and load extensions from standard locations.
 */
export async function discoverAndLoadExtensions(
	configuredPaths: string[],
	cwd: string,
	agentDir: string = getAgentDir(),
	eventBus?: EventBus,
): Promise<LoadExtensionsResult> {
	const allPaths: string[] = [];
	const seen = new Set<string>();

	const addPaths = (paths: string[]) => {
		for (const p of paths) {
			const resolved = path.resolve(p);
			if (!seen.has(resolved)) {
				seen.add(resolved);
				allPaths.push(p);
			}
		}
	};

	// 1. Project-local extensions: cwd/.pi/extensions/
	const localExtDir = path.join(cwd, ".pi", "extensions");
	addPaths(discoverExtensionsInDir(localExtDir));

	// 2. Global extensions: agentDir/extensions/
	const globalExtDir = path.join(agentDir, "extensions");
	addPaths(discoverExtensionsInDir(globalExtDir));

	// 3. Explicitly configured paths
	for (const p of configuredPaths) {
		const resolved = resolvePath(p, cwd);
		if (fs.existsSync(resolved) && fs.statSync(resolved).isDirectory()) {
			// Check for package.json with pi manifest or index.ts
			const entries = resolveExtensionEntries(resolved);
			if (entries) {
				addPaths(entries);
				continue;
			}
			// No explicit entries - discover individual files in directory
			addPaths(discoverExtensionsInDir(resolved));
			continue;
		}

		addPaths([resolved]);
	}

	return loadExtensions(allPaths, cwd, eventBus);
}
