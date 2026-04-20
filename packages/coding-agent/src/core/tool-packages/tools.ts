/**
 * Tool registry + resolver.
 *
 * Given:
 *   - the set of registered providers,
 *   - the user-authored `ToolsConfig` (from `pi.configureTools(...)`),
 *   - resolvers for config + secrets,
 *
 * produce the final `ToolDefinition[]` that the agent session wires into
 * the LLM.
 *
 * Resolution rules:
 *
 *   1. If the user has an entry in `ToolsConfig`, that entry wins. The key
 *      becomes the LLM-visible tool name; the `provider` field picks which
 *      registered provider to execute.
 *   2. For every registered interface *not* overridden by (1):
 *        * 0 providers  -> skip.
 *        * 1 provider   -> auto-bind under the bare interface name.
 *        * 2+ providers -> **throw** a clear resolve-time error listing the
 *                           provider ids and telling the user to add a
 *                           `tools.<name> = { provider: "..." }` entry.
 *          (No silent first-wins. Prevents the LLM from seeing an
 *          unpredictable default when multiple extensions register the
 *          same interface.)
 *
 * Diagnostics:
 *   - `ToolsConfig` entry referencing an unknown provider id  -> throw.
 *   - `ToolsConfig` entry whose provider doesn't implement any registered
 *     interface                                               -> warn + skip.
 *   - Multiple calls to `configureTools(...)` merge at the tool-name level
 *     (later call wins, with a warning).
 */

import type {
	SecretSpec,
	ToolCallContext,
	ToolConfigEntry,
	ToolHost,
	ToolInterface,
	ToolProvider,
	ToolResult,
	ToolsConfig,
	ToolUpdateCallback,
} from "@pi-relay/tool-kit";
import { ToolConfigMissingError } from "@pi-relay/tool-kit";
import type { ExtensionContext, ToolDefinition } from "../extensions/types.js";
import { ToolInterfaceRegistry } from "./interfaces.js";

/** Resolve the merged config for a tool entry (defaults + per-tool override). */
export type ResolveToolConfig = (args: {
	toolName: string;
	providerId: string;
	provider: ToolProvider;
	entry: ToolConfigEntry | undefined;
}) => unknown;

/** Resolve secrets for a provider given its declared specs. */
export type ResolveProviderSecrets = (
	providerId: string,
	specs: readonly SecretSpec[],
) => Record<string, string | undefined>;

/** Build a `ToolHost` from an `ExtensionContext`. */
export type HostFactory = (ctx: ExtensionContext) => ToolHost;

/** Record of a registered provider. Stored in the registry. */
export interface RegisteredProvider {
	provider: ToolProvider;
	/** Path of the extension that registered the provider. */
	extensionPath?: string;
}

export interface ToolRegistryOptions {
	interfaces?: ToolInterfaceRegistry;
	resolveConfig?: ResolveToolConfig;
	resolveSecrets?: ResolveProviderSecrets;
	hostFactory: HostFactory;
	/** Sink for warnings (unknown interface / unbound provider / merged tool config). */
	warn?: (message: string) => void;
}

/** One resolved tool, ready to be turned into a `ToolDefinition`. */
export interface ResolvedTool {
	toolName: string;
	iface: ToolInterface;
	provider: ToolProvider;
	entry: ToolConfigEntry | undefined;
}

/**
 * Default config resolver: returns the per-tool `entry.config`. Provider
 * defaults are merged in separately by `buildDefinition`.
 */
export function defaultResolveToolConfig(args: Parameters<ResolveToolConfig>[0]): unknown {
	return args.entry?.config ?? undefined;
}

/** Default secret resolver: reads each declared secret from `process.env[spec.envVar]`. */
export function defaultResolveProviderSecrets(
	_providerId: string,
	specs: readonly SecretSpec[],
): Record<string, string | undefined> {
	const out: Record<string, string | undefined> = {};
	for (const spec of specs) {
		out[spec.key] = spec.envVar ? process.env[spec.envVar] : undefined;
	}
	return out;
}

function mergeShallow(a: unknown, b: unknown): unknown {
	if (a === undefined || a === null) return b ?? {};
	if (b === undefined || b === null) return a;
	if (
		typeof a === "object" &&
		typeof b === "object" &&
		!Array.isArray(a) &&
		!Array.isArray(b)
	) {
		return { ...(a as Record<string, unknown>), ...(b as Record<string, unknown>) };
	}
	return b;
}

/**
 * Registry of providers + user-authored `ToolsConfig`, producing
 * `ToolDefinition`s.
 *
 * Lifetime:
 *   - One `ToolRegistry` is created per `ExtensionAPI` (on the runtime)
 *     and accumulates provider registrations during extension load.
 *   - `resolve()` is called whenever the tool registry needs to be
 *     recomputed (any `pi.registerToolProvider` / `pi.configureTools` call
 *     triggers `runtime.refreshTools()`).
 */
export class ToolRegistry {
	private readonly providers = new Map<string, RegisteredProvider>();
	private readonly config: ToolsConfig = {};
	private readonly options: Required<Omit<ToolRegistryOptions, "interfaces">> & {
		interfaces: ToolInterfaceRegistry;
	};

	constructor(options: ToolRegistryOptions) {
		this.options = {
			interfaces: options.interfaces ?? new ToolInterfaceRegistry(),
			resolveConfig: options.resolveConfig ?? defaultResolveToolConfig,
			resolveSecrets: options.resolveSecrets ?? defaultResolveProviderSecrets,
			hostFactory: options.hostFactory,
			warn: options.warn ?? ((m) => console.warn(m)),
		};
	}

	/** Register a provider. Ignores duplicate ids with a warning. */
	registerProvider(provider: ToolProvider, extensionPath?: string): void {
		if (!provider.id || typeof provider.id !== "string") {
			throw new Error("registerToolProvider: provider.id must be a non-empty string.");
		}
		if (!provider.implements || typeof provider.implements !== "string") {
			throw new Error(
				`registerToolProvider: provider "${provider.id}" must declare a non-empty "implements" interface name.`,
			);
		}
		if (this.providers.has(provider.id)) {
			this.options.warn(
				`registerToolProvider: duplicate provider id "${provider.id}"; ignoring second registration.`,
			);
			return;
		}
		if (!this.options.interfaces.has(provider.implements)) {
			this.options.warn(
				`registerToolProvider: provider "${provider.id}" implements unknown interface "${provider.implements}". It cannot be bound to a tool until the interface is registered.`,
			);
		}
		this.providers.set(provider.id, { provider, extensionPath });
	}

	/** True if a provider with the given id has been registered. */
	hasProvider(providerId: string): boolean {
		return this.providers.has(providerId);
	}

	listProviders(): RegisteredProvider[] {
		return Array.from(this.providers.values());
	}

	/**
	 * Merge a user-authored `ToolsConfig` into the registry. Per-tool-name
	 * merge: if a name is already configured, the new entry replaces it and
	 * a warning is emitted.
	 */
	configureTools(config: ToolsConfig): void {
		for (const [name, entry] of Object.entries(config)) {
			if (!entry || typeof entry !== "object" || typeof entry.provider !== "string" || !entry.provider) {
				throw new Error(
					`configureTools: tool entry "${name}" must be an object with a non-empty "provider" string.`,
				);
			}
			if (this.config[name] && this.config[name].provider !== entry.provider) {
				this.options.warn(
					`configureTools: tool "${name}" was previously bound to "${this.config[name].provider}"; overriding with "${entry.provider}".`,
				);
			}
			this.config[name] = entry;
		}
	}

	/** Read-only view of the currently configured tools. */
	getConfig(): Readonly<ToolsConfig> {
		return this.config;
	}

	/**
	 * Resolve all registered providers + `ToolsConfig` into final
	 * `ToolDefinition`s. See the rules in this file's header comment.
	 *
	 * Throws on:
	 *   - `ToolsConfig` entry whose `provider` id is unknown.
	 *   - Interface with >1 registered providers that has no explicit
	 *     tool-config override.
	 */
	resolve(): ToolDefinition[] {
		const resolved: ResolvedTool[] = [];
		// Providers claimed by explicit tool entries, so we can exclude them
		// from the auto-bind pass.
		const claimedProviders = new Set<string>();

		// Pass 1: user-configured tools.
		for (const [toolName, entry] of Object.entries(this.config)) {
			const rp = this.providers.get(entry.provider);
			if (!rp) {
				throw new Error(
					`configureTools: tool "${toolName}" references unknown provider "${entry.provider}". Registered providers: ${this.listProviderIds().join(", ") || "(none)"}.`,
				);
			}
			const iface = this.options.interfaces.get(rp.provider.implements);
			if (!iface) {
				this.options.warn(
					`configureTools: tool "${toolName}" uses provider "${rp.provider.id}", which implements unknown interface "${rp.provider.implements}"; skipping.`,
				);
				continue;
			}
			claimedProviders.add(rp.provider.id);
			resolved.push({ toolName, iface, provider: rp.provider, entry });
		}

		// Pass 2: auto-bind interfaces the user didn't explicitly configure.
		const providersByInterface = new Map<string, RegisteredProvider[]>();
		for (const rp of this.providers.values()) {
			if (claimedProviders.has(rp.provider.id)) continue;
			if (!this.options.interfaces.has(rp.provider.implements)) continue;
			const list = providersByInterface.get(rp.provider.implements) ?? [];
			list.push(rp);
			providersByInterface.set(rp.provider.implements, list);
		}

		// Skip auto-binding for an interface name that the user already
		// reserved under pass 1 (e.g. user wrote `tools.web_search = {...}`).
		const explicitToolNames = new Set(Object.keys(this.config));

		for (const [interfaceName, providersForInterface] of providersByInterface) {
			if (explicitToolNames.has(interfaceName)) continue;

			if (providersForInterface.length === 1) {
				const rp = providersForInterface[0];
				const iface = this.options.interfaces.get(interfaceName);
				if (!iface) continue;
				resolved.push({ toolName: interfaceName, iface, provider: rp.provider, entry: undefined });
			} else {
				// >1 providers for the same interface with no user-chosen
				// tool entry. Fail fast instead of picking one silently.
				const ids = providersForInterface.map((r) => `"${r.provider.id}"`).join(", ");
				throw new Error(
					`Multiple providers implement "${interfaceName}" (${ids}) and no \`tools\` entry disambiguates them. ` +
						`Add \`pi.configureTools({ ${interfaceName}: { provider: "<id>" } })\` or declare named tool entries ` +
						`(e.g. \`tools.${interfaceName}_alt = { provider: "<id>" }\`) to select which provider backs which tool.`,
				);
			}
		}

		// Defensive: the Record shape prevents duplicate tool names in user
		// config, but verify the merged list doesn't collide.
		const seenNames = new Set<string>();
		for (const r of resolved) {
			if (seenNames.has(r.toolName)) {
				throw new Error(
					`Tool resolver produced duplicate tool name "${r.toolName}". This is a pi-relay bug; please report it.`,
				);
			}
			seenNames.add(r.toolName);
		}

		return resolved.map((r) => this.buildDefinition(r));
	}

	private listProviderIds(): string[] {
		return Array.from(this.providers.keys());
	}

	private buildDefinition(r: ResolvedTool): ToolDefinition {
		const { iface, provider, entry, toolName } = r;
		const parameters = provider.parameters ?? iface.parameters;
		// LLM-facing description is ALWAYS the interface description. Provider
		// descriptions would leak the provider id/name (e.g. "via Perplexity
		// Sonar") into the system prompt and defeat the point of keeping
		// provider selection purely user-facing. `provider.description` is still
		// kept on the ToolProvider record for diagnostics / `/tools` listings.
		const description = iface.description;
		const promptSnippet = provider.promptSnippet ?? iface.promptSnippet;
		const promptGuidelines = [...(iface.promptGuidelines ?? []), ...(provider.promptGuidelines ?? [])];
		const options = this.options;

		const def: ToolDefinition = {
			name: toolName,
			label: iface.name === toolName ? iface.name : `${iface.name} (${toolName})`,
			description,
			promptSnippet,
			promptGuidelines: promptGuidelines.length > 0 ? promptGuidelines : undefined,
			parameters,
			renderShell: provider.renderShell,
			prepareArguments: provider.prepareArguments,
			// biome-ignore lint/suspicious/noExplicitAny: cross-boundary renderer; typed via render subpath on the author side.
			renderCall: provider.renderCall as any,
			// biome-ignore lint/suspicious/noExplicitAny: cross-boundary renderer; typed via render subpath on the author side.
			renderResult: provider.renderResult as any,
			async execute(toolCallId, params, signal, onUpdate, ctx) {
				const configOverrides = options.resolveConfig({
					toolName,
					providerId: provider.id,
					provider,
					entry,
				});
				const config = mergeShallow(provider.defaultConfig, configOverrides);
				const secrets = resolveAndValidateSecrets(provider, options.resolveSecrets);
				const host = options.hostFactory(ctx);
				const toolCtx: ToolCallContext<unknown, unknown, Record<string, string | undefined>> = {
					toolCallId,
					cwd: ctx.cwd,
					signal,
					onUpdate: onUpdate as ToolUpdateCallback | undefined,
					config,
					secrets,
					host,
					params,
					toolName,
				};
				// The author-side generic signature is erased at the registry
				// boundary so the resolver can build one closure per tool
				// without threading generics through the extension runtime.
				// biome-ignore lint/suspicious/noExplicitAny: erasing author generics at the tool boundary.
				const anyExecute = provider.execute as (p: unknown, c: unknown) => Promise<ToolResult<any>>;
				const result = await anyExecute(params, toolCtx);
				return result;
			},
		};
		return def;
	}
}

function resolveAndValidateSecrets(
	provider: ToolProvider,
	resolver: ResolveProviderSecrets,
): Record<string, string | undefined> {
	const specs = provider.secrets ?? [];
	const resolved = resolver(provider.id, specs);
	const missing: string[] = [];
	for (const spec of specs) {
		if (spec.optional) continue;
		if (typeof resolved[spec.key] !== "string" || resolved[spec.key] === "") {
			missing.push(spec.key);
		}
	}
	if (missing.length > 0) {
		const hints = specs
			.filter((s) => missing.includes(s.key) && s.envVar)
			.map((s) => `${s.key} (set ${s.envVar})`);
		const hint = hints.length > 0 ? `Try: ${hints.join(", ")}.` : undefined;
		throw new ToolConfigMissingError(provider.id, missing, hint);
	}
	return resolved;
}
