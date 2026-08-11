// prime-rlm role-configured spawn support (M7) — pi-relay subagent-role parity.
//
// PROVENANCE: spawn semantics ported from pi-relay
//   rust/agent-daemon/src/subagents.rs (select_subagent_provider:
//   explicit caller model -> role frontmatter model -> parent default) and
//   rust/agent-runtime/src/skills.rs (role validation doctrine; an unusable
//   role is omitted from the catalog and errors at spawn).
//
// Role DISCOVERY + storage lives in prime-harness (roles are continual-harness
// content). prime-rlm consumes it through the documented public seam
// `prime-harness.host-api` v1 (lazy globalThis lookup, load-order tolerant —
// same pattern as the prime-comms seam in rlm-host.ts). An rlm-only load (C3
// modularity) has no role resolution: rlm.run(role=...) then fails with a
// clear error, plain rlm.run is unaffected.

export const PRIME_HARNESS_API_SYMBOL = "prime-harness.host-api";

/** Role snapshot passed across the prime-harness -> prime-rlm seam at spawn
 * and stored on the child's SessionState (registry.ts). Structural mirror of
 * prime-harness's ResolvedRole — the two packages intentionally share no code. */
export interface SessionRoleSpec {
	name: string;
	description: string;
	/** Display path: absolute SKILL.md path, or `harness:<scope>:<entry-id>`
	 * for roles stored as harness entries. */
	filePath: string;
	/** SKILL.md body (frontmatter stripped) — inlined into the child prompt. */
	body: string;
	/** Raw frontmatter model selector. New-stack canonical form is
	 * "provider/model-id"; pi-relay's legacy "provider:model" is also accepted
	 * (parsed by parseRoleModelSelector). */
	model?: string;
	/** pi-relay `reasoning_effort` frontmatter (alias `effort` accepted by the
	 * harness-side parser). Validated against ThinkingLevel at spawn. */
	reasoningEffort?: string;
	/** pi-relay `max_tokens` frontmatter: overrides the resolved model's
	 * maxTokens for the child session. */
	maxTokens?: number;
	/** Preloaded skills (pi-relay `skills` frontmatter, alias `preload`).
	 * Content is the SKILL.md body, inlined into the child prompt; python
	 * skills are additionally pre-imported into the child kernel. */
	preload: Array<{
		name: string;
		filePath: string;
		content: string;
		pythonImport?: string;
		pythonPath?: string;
	}>;
	/** Discovery origin label (diagnostics): "harness-local" | "project-dir" |
	 * "harness-global" | "global-dir". */
	origin?: string;
}

/** Context the harness side needs to resolve roles for a spawning session:
 * harness-local roles live under the session's own harness store. */
export interface HarnessRoleContext {
	sessionId: string;
	sessionDir: string;
	cwd: string;
	agentDir: string;
}

interface PrimeHarnessRolesApi {
	version: number;
	/** Valid roles visible to the session (name+description, sorted by name). */
	roleCatalog(ctx: HarnessRoleContext): Array<{ name: string; description: string }>;
	/** Fully resolve a role for spawn. Throws when the name is unknown or the
	 * role is invalid (message explains why and lists known roles). */
	resolveRole(name: string, ctx: HarnessRoleContext): SessionRoleSpec;
}

/** Lazy lookup — prime-harness may load AFTER prime-rlm (settings.json order),
 * so the seam is read at spawn time, never at module load. */
export function getHarnessRolesApi(): PrimeHarnessRolesApi | undefined {
	const candidate = (globalThis as Record<symbol, unknown>)[Symbol.for(PRIME_HARNESS_API_SYMBOL)];
	if (candidate && (candidate as { version?: unknown }).version === 1) {
		return candidate as PrimeHarnessRolesApi;
	}
	return undefined;
}

/** Peer-extension detection for prompt assembly (PA's hasAgentMessage /
 * hasAgentObserve / includeRefineExamples flags — PA reads its installed-skills
 * list; here the presence of the sibling seam is the equivalent signal).
 * prime-comms provides agent_message AND agent_observe; prime-harness provides
 * refine.run and the role catalog. */
export function detectPeerExtensions(): { comms: boolean; harness: boolean } {
	const comms = (globalThis as Record<symbol, unknown>)[Symbol.for("prime-comms.host-api")];
	return {
		comms: !!comms && (comms as { version?: unknown }).version === 1,
		harness: getHarnessRolesApi() !== undefined,
	};
}

/** pi ThinkingLevel values (pi-agent-core). Kept as a local list so this module
 * stays free of pi-agent-core type imports. */
export const VALID_THINKING_LEVELS = new Set(["off", "minimal", "low", "medium", "high", "xhigh", "max"]);

/** Parse a role frontmatter model selector.
 * Canonical new-stack form: "provider/model-id" (model ids may themselves
 * contain "/", e.g. "nvidia-inference/nvidia/zai-org/glm-5.2" — provider is the
 * first segment). pi-relay's legacy "provider:model" (subagents.rs
 * role_provider_from_frontmatter) is accepted for ported role files. */
export function parseRoleModelSelector(raw: string): { provider: string; id: string } | undefined {
	const trimmed = raw.trim();
	if (!trimmed || trimmed !== raw) return undefined;
	if (trimmed.includes("/")) {
		const [provider, ...rest] = trimmed.split("/");
		const id = rest.join("/");
		return provider && id ? { provider, id } : undefined;
	}
	const i = trimmed.indexOf(":");
	if (i <= 0 || i === trimmed.length - 1) return undefined;
	const provider = trimmed.slice(0, i);
	const id = trimmed.slice(i + 1);
	if (id.includes(":")) return undefined; // pi-relay: native_model.contains(':') is malformed
	return { provider, id };
}

export interface ResolvedRoleSpawn {
	role: SessionRoleSpec;
	/** Model override to apply (undefined = inherit parent model). */
	modelOverride?: { provider: string; id: string };
	thinkingLevel?: string;
	maxTokens?: number;
	/** Set when the role asked for a model the registry does not have: pi-relay
	 * falls back to its stable default provider with a warning; the new-stack
	 * analogue is falling back to the PARENT model (documented in M7-ROLES.md). */
	modelFallbackReason?: string;
}

/** Resolve a role spawn: discovery via the prime-harness seam, then model /
 * effort / max_tokens policy (select_subagent_provider port). Throws on:
 * seam missing, unknown role (pi-relay role_not_found), invalid role. */
export function resolveRoleForSpawn(
	roleName: string,
	ctxInfo: HarnessRoleContext,
	findModel: (provider: string, id: string) => boolean,
	warn: (message: string) => void = (m) => console.warn(m),
): ResolvedRoleSpawn {
	const api = getHarnessRolesApi();
	if (!api) {
		throw new Error(
			"rlm() role=... requires the prime-harness extension (prime-harness.host-api v1 seam not found); omit role or load prime-harness",
		);
	}
	const role = api.resolveRole(roleName, ctxInfo); // throws role-not-found / invalid-role
	const out: ResolvedRoleSpawn = { role };
	if (role.model) {
		const selector = parseRoleModelSelector(role.model);
		if (!selector) {
			throw new Error(
				`role "${role.name}" has malformed model '${role.model}'; expected "provider/model-id" (or pi-relay legacy "provider:model")`,
			);
		}
		if (findModel(selector.provider, selector.id)) {
			out.modelOverride = selector;
			// pi-relay: reasoning_effort defaults to "medium" once a role sets a model.
			const effort = (role.reasoningEffort ?? "medium").trim();
			if (VALID_THINKING_LEVELS.has(effort)) {
				out.thinkingLevel = effort;
			} else {
				warn(
					`[prime-rlm] role "${role.name}" reasoning_effort '${effort}' is not a pi thinking level; using "medium"`,
				);
				out.thinkingLevel = "medium";
			}
			if (role.maxTokens !== undefined) out.maxTokens = role.maxTokens;
		} else {
			out.modelFallbackReason = `role model '${role.model}' not in the model registry; falling back to the parent model`;
			warn(`[prime-rlm] ${out.modelFallbackReason}`);
		}
	}
	return out;
}
