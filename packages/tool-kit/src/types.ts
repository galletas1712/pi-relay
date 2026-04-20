/**
 * Author-facing types for pi-relay tool-kit.
 *
 * Two-layer model:
 *
 *   ToolInterface   - the contract (name, description, parameters). Defined
 *                     by pi-relay core for built-ins; third parties can
 *                     declare their own via `defineToolInterface`.
 *   ToolProvider    - a pluggable implementation of a `ToolInterface` with
 *                     its own config schema + secrets + `execute`. Users
 *                     never see provider ids in LLM-visible tool names.
 *   ToolsConfig     - user-authored map of LLM-visible tool names to the
 *                     chosen provider (and optional per-tool config). This
 *                     is the first layer: one tool name => one provider.
 *
 * Example: a user who wants a local shell AND a remote production shell
 * declares two tools with DIFFERENT NAMES, both pointing at different
 * providers that implement the `bash` interface:
 *
 *   pi.configureTools({
 *     bash:      { provider: "local" },
 *     bash_prod: { provider: "ssh", config: { host: "prod.example.com" } },
 *   });
 *
 * The LLM sees `bash` and `bash_prod`. The provider ids (`local`, `ssh`)
 * are invisible to the model.
 *
 * This module has no runtime dependency on any other @pi-relay/* package,
 * so third-party authors can write a provider with only @sinclair/typebox
 * and @pi-relay/tool-kit on their import graph.
 *
 * Optional custom rendering uses `@pi-relay/tool-kit/render`, which
 * re-exports TUI primitives. Providers that don't need custom UI need not
 * import it.
 */

import type { Static, TSchema } from "@sinclair/typebox";

// ============================================================================
// Tool I/O primitives (mirrored here to avoid pulling agent-core types into
// author packages). The coding-agent adapter maps these 1:1 onto
// AgentToolResult / AgentToolUpdateCallback.
// ============================================================================

/** Text content returned to the model. */
export interface ToolTextContent {
	type: "text";
	text: string;
}

/** Image content returned to the model (base64-encoded). */
export interface ToolImageContent {
	type: "image";
	/** IANA media type, e.g. "image/png". */
	mimeType: string;
	/** Base64-encoded image bytes. */
	data: string;
}

export type ToolContent = ToolTextContent | ToolImageContent;

/** Final or partial result produced by a tool's `execute`. */
export interface ToolResult<TDetails = unknown> {
	/** Content sent back to the model. */
	content: ToolContent[];
	/** Arbitrary structured details for logs / UI rendering. */
	details: TDetails;
}

/** Callback used by providers to stream partial results. */
export type ToolUpdateCallback<TDetails = unknown> = (partial: ToolResult<TDetails>) => void;

// ============================================================================
// Host surface (provided to providers at call time via ToolCallContext.host).
// ============================================================================

/** A minimal reference to the currently-selected model, without leaking @pi-relay/ai types. */
export interface ToolHostModelRef {
	/** Model id, e.g. "gpt-5-codex". */
	id: string;
	/** Provider id, e.g. "openai-codex", "anthropic". */
	provider: string;
	/**
	 * The host's native `Model` object (typed as `unknown` here to keep the
	 * author-facing surface free of `@pi-relay/ai` types). In-tree providers
	 * that must interop with `@pi-relay/ai` APIs like `completeSimple` may
	 * cast this to `Model<Api>`. Most third-party providers should use
	 * `host.http` instead.
	 */
	native?: unknown;
}

/** Result of resolving an API key + headers for a provider. */
export type ToolHostAuth =
	| { ok: true; apiKey?: string; headers?: Record<string, string> }
	| { ok: false; error: string };

/**
 * Host-provided services a provider may use. Kept intentionally small; richer
 * services can be added in later milestones without breaking callers.
 */
export interface ToolHost {
	/** The currently-selected model in the session, if any. */
	getModel(): ToolHostModelRef | undefined;
	/**
	 * Resolve auth for the given provider id (e.g. "openai-codex"). Used when a
	 * provider wants to piggyback on an existing model credential instead of
	 * owning its own secret. The coding-agent host routes this through
	 * ModelRegistry.getApiKeyAndHeaders for the first model matching `provider`.
	 */
	getApiKey(provider: string): Promise<ToolHostAuth>;
	/** HTTP client. Defaults to the Node `fetch` global. */
	http: typeof fetch;
	/**
	 * Host-specific escape hatch for providers that need access to an object
	 * the public surface doesn't yet expose (e.g. a Session object, a process
	 * handle). Typed as `unknown` so tool-kit can remain dependency-free and
	 * authors must opt in with a deliberate cast. Avoid using in third-party
	 * providers - prefer `http` + `getApiKey` + env-backed secrets.
	 */
	native?: Record<string, unknown>;
}

// ============================================================================
// Secrets
// ============================================================================

/** Kind of secret value; informs UI rendering and masking, not storage. */
export type SecretKind = "api_key" | "oauth_token" | "password" | "string";

/**
 * Declaration of a secret the provider needs at runtime.
 *
 * In this milestone, secret values are resolved from `envVar` only. A later
 * milestone will add an AuthStorage-backed `tools.<toolName>` namespace and
 * a `/configure <toolName>` UI.
 */
export interface SecretSpec {
	/** Identifier used on `ToolCallContext.secrets`. Must be a valid JS property name. */
	key: string;
	/** Human-readable name shown in configuration UI. */
	displayName: string;
	/** How to render / collect the value. */
	kind: SecretKind;
	/**
	 * Environment variable to read as a fallback. Required in milestone 1
	 * because there is no other storage yet; marked optional so later
	 * milestones can introduce alternative storage.
	 */
	envVar?: string;
	/** Optional help text for configuration UI. */
	description?: string;
	/** If true, the provider still runs without this secret; missing secret is `undefined`. */
	optional?: boolean;
}

/**
 * Callbacks used when a provider performs an OAuth-style login. Kept here as
 * a stable contract so later milestones can plumb the host's real prompt UI
 * through without touching provider code. Mirrors @pi-relay/ai's
 * OAuthLoginCallbacks shape.
 */
export interface LoginCallbacks {
	onAuth: (info: { url: string; instructions?: string }) => void;
	onPrompt: (prompt: { message: string; placeholder?: string; allowEmpty?: boolean }) => Promise<string>;
	onProgress?: (message: string) => void;
	onManualCodeInput?: () => Promise<string>;
	signal?: AbortSignal;
}

// ============================================================================
// Tool call context (passed to ToolProvider.execute at runtime)
// ============================================================================

export interface ToolCallContext<TParams, TConfig = Record<string, never>, TSecrets = Record<string, never>> {
	/** The tool call id from the model. */
	toolCallId: string;
	/** Session working directory. */
	cwd: string;
	/** Abort signal for the current turn, if any. */
	signal: AbortSignal | undefined;
	/** Streaming update callback (may be undefined in non-interactive contexts). */
	onUpdate: ToolUpdateCallback | undefined;
	/** Resolved, typed config values merged over provider `defaultConfig`. */
	config: TConfig;
	/**
	 * Resolved, typed secret values. Each declared `SecretSpec.key` appears
	 * here; optional secrets may be `undefined` at runtime (typed via
	 * TSecrets).
	 */
	secrets: TSecrets;
	/** Host-provided services (model, auth, http). */
	host: ToolHost;
	/** Original, validated tool parameters (duplicated for convenience). */
	params: TParams;
	/**
	 * The user-chosen, LLM-visible tool name this call arrived under. For
	 * auto-bound single-provider interfaces this equals the interface name;
	 * otherwise it is the key the user wrote in `pi.configureTools({...})`
	 * (e.g. `"bash_prod"`). Useful for logs and diagnostics.
	 */
	toolName: string;
}

// ============================================================================
// ToolInterface
// ============================================================================

/**
 * Renderer types are intentionally untyped here so the main entry is TUI-free.
 * Authors who want custom rendering should import the concrete Component
 * types from `@pi-relay/tool-kit/render` and cast their renderers, or use
 * `as unknown as ToolRenderer` locally. The adapter forwards these values
 * opaquely to the underlying `ToolDefinition`.
 */
export type ToolRenderer = (...args: unknown[]) => unknown;

/**
 * Contract that providers implement. The LLM sees a tool whose name,
 * description, and parameters come from (or default to) the interface.
 *
 * Interfaces are usually declared by pi-relay core (e.g. `web_search`,
 * `bash`) but third parties may declare their own. Provider authors
 * reference an interface by name via `ToolProvider.implements`.
 */
export interface ToolInterface<TParams extends TSchema = TSchema, TResult extends TSchema = TSchema> {
	/** Interface name. Auto-used as the tool name when a single provider is registered and the user hasn't overridden via `pi.configureTools(...)`. */
	name: string;
	/** Description sent to the LLM (providers may override per-tool). */
	description: string;
	/** Parameter schema (TypeBox). Providers may extend it, not shrink it. */
	parameters: TParams;
	/**
	 * Optional TypeBox schema documenting the expected result shape. Not
	 * enforced at runtime today; serves as documentation / future validation.
	 */
	resultShape?: TResult;
	/** Optional one-line snippet for the default system-prompt tool list. */
	promptSnippet?: string;
	/** Optional guideline bullets appended when this tool is active. */
	promptGuidelines?: string[];
}

// ============================================================================
// ToolProvider
// ============================================================================

/**
 * A single pluggable implementation of a `ToolInterface`.
 *
 * Design choice: one provider per package. Earlier drafts allowed a package
 * to ship multiple tools, but per-provider config vs. per-package config
 * collides when a single package declares multiple providers that implement
 * different interfaces. "One extension file = one provider" is simpler and
 * maps cleanly to the user-facing tool configuration.
 *
 * Provider ids are internal \u2014 the LLM never sees them. They surface only
 * in `pi.configureTools({ <toolName>: { provider: "<id>" } })`, log
 * messages, and diagnostics.
 */
export interface ToolProvider<
	TConfig = Record<string, never>,
	TSecrets = Record<string, never>,
	TParams extends TSchema = TSchema,
	TDetails = unknown,
> {
	/** Globally-unique provider id, e.g. "com.perplexity.sonar". */
	id: string;
	/** Human-readable display name for diagnostics / `/tools` / UI. */
	displayName: string;
	/** Semver string. */
	version: string;
	/** Name of the interface this provider implements (e.g. "web_search"). */
	implements: string;
	/** Optional override for the interface description, sent to the LLM. */
	description?: string;
	/** Optional override for the interface prompt snippet. */
	promptSnippet?: string;
	/** Optional extra guideline bullets appended when this provider is active. */
	promptGuidelines?: string[];
	/**
	 * Optional parameter schema override. When omitted, the resolver uses the
	 * interface's `parameters`. Providers that add optional fields (e.g.
	 * `recency` on `web_search`) declare them here.
	 */
	parameters?: TParams;
	/** Controls whether the TUI renders the standard colored shell around the call. */
	renderShell?: "default" | "self";
	/** Optional compatibility shim to normalize raw args before schema validation. */
	prepareArguments?: (args: unknown) => Static<TParams>;
	/** TypeBox schema validating `config` (documentation; not enforced today). */
	configSchema?: TSchema;
	/** Defaults merged under any user-provided per-tool config. */
	defaultConfig?: TConfig;
	/** Declared secrets (resolved via env vars in milestone 1). */
	secrets?: readonly SecretSpec[];
	/** Run the tool call. Throw on failure. */
	execute(
		params: Static<TParams>,
		ctx: ToolCallContext<Static<TParams>, TConfig, TSecrets>,
	): Promise<ToolResult<TDetails>>;
	/** Optional custom renderer for the tool call. Typed loosely; use `@pi-relay/tool-kit/render` for real types. */
	renderCall?: ToolRenderer;
	/** Optional custom renderer for the tool result. Typed loosely; use `@pi-relay/tool-kit/render` for real types. */
	renderResult?: ToolRenderer;
	/**
	 * Reserved for a later milestone that adds an init/dispose lifecycle.
	 * Declared here so authors can reference the type today without a
	 * breaking change later; the adapter does not call it yet.
	 */
	init?: (ctx: ToolProviderInitContext<TConfig, TSecrets>) => void | Promise<void>;
	/** Reserved for a later milestone; not called today. */
	dispose?: () => void | Promise<void>;
}

/**
 * Context passed to `ToolProvider.init` (reserved for a later milestone).
 * Kept here as a stable type so authors can reference it today.
 */
export interface ToolProviderInitContext<TConfig = Record<string, never>, TSecrets = Record<string, never>> {
	config: TConfig;
	secrets: TSecrets;
	host: ToolHost;
	signal: AbortSignal | undefined;
}

// ============================================================================
// Tools configuration (first layer)
// ============================================================================

/**
 * One entry in `ToolsConfig`. Picks the provider and (optionally) overrides
 * its `defaultConfig`.
 */
export interface ToolConfigEntry<TConfig = unknown> {
	/** Provider id to back this tool. Must match a registered `ToolProvider.id`. */
	provider: string;
	/** Per-tool config overlaid on top of the provider's `defaultConfig`. */
	config?: Partial<TConfig>;
}

/**
 * User-authored map of LLM-visible tool names to provider selections.
 *
 * The key (e.g. `"bash"`, `"bash_prod"`, `"web_search"`) is what the LLM
 * sees; the value binds it to one provider.
 */
export type ToolsConfig = Record<string, ToolConfigEntry>;
