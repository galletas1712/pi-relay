// Bridge configuration: everything env-driven. Secrets arrive via the process
// environment (run-bridge.sh pipes them in); nothing secret is ever logged.
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const pkgRoot = path.resolve(here, "..");
const repoRoot = path.resolve(pkgRoot, "..", "..");
const demoDir = path.join(repoRoot, ".pi", "m1-demo");

function resolvePiCli(): string {
	// ESM resolution honors the package's "import" export condition.
	const entry = fileURLToPath(import.meta.resolve("@earendil-works/pi-coding-agent"));
	return path.join(path.dirname(entry), "cli.js");
}

function env(name: string, fallback?: string): string {
	const v = process.env[name];
	if (v !== undefined && v !== "") return v;
	if (fallback !== undefined) return fallback;
	throw new Error(`missing required env ${name}`);
}

/** M10b: validated BRIDGE_PI_CACHE_RETENTION. Unset/empty → null (hosts keep
 * upstream pi-ai default "short"); anything else must be a literal
 * CacheRetention. Exported for unit tests. */
export function parseCacheRetention(v: string): "none" | "short" | "long" | null {
	if (v === "") return null;
	if (v === "none" || v === "short" || v === "long") return v;
	throw new Error(`invalid BRIDGE_PI_CACHE_RETENTION ${JSON.stringify(v)} (expected none|short|long)`);
}

export const config = {
	pkgRoot,
	repoRoot,
	/** WS listen port */
	port: Number(env("BRIDGE_PORT", "8730")),
	/** Bearer token required at WS upgrade (Authorization: Bearer ...) */
	authToken: env("BRIDGE_AUTH_TOKEN"),
	/** Comma-separated Origin allowlist; exact match, missing Origin rejected */
	allowedOrigins: env("BRIDGE_ALLOWED_ORIGINS", "http://localhost:3000")
		.split(",")
		.map((s) => s.trim())
		.filter(Boolean),
	/** Max WS frame/message size (old contract discipline: 8 MiB) */
	maxFrameBytes: Number(env("BRIDGE_MAX_FRAME_BYTES", String(8 * 1024 * 1024))),
	/** Postgres control-plane DSN (TEST container 127.0.0.1:56432 ONLY) */
	pgUrl: env("BRIDGE_PG_URL"),
	/** pi CLI entry (pinned @earendil-works/pi-coding-agent; resolved through node
	 * so workspace hoisting is transparent) */
	piCli: env("BRIDGE_PI_CLI", resolvePiCli()),
	/** Agent dir with settings.json loading the 4 prime extensions + models.json */
	agentDir: env("PI_CODING_AGENT_DIR", path.join(demoDir, "agent")),
	/** Kernel venv provisioned by M1 (dill/nest_asyncio/httpx/Pillow) */
	kernelVenv: env("PRIME_RLM_KERNEL_VENV", path.join(demoDir, "venv")),
	/** Bridge data dir: sessions/, journal/, logs/, scratch/ */
	dataDir: env("BRIDGE_DATA_DIR", path.join(pkgRoot, "data")),
	/** Default cwd for new session hosts (model file writes stay contained) */
	defaultCwd: env("BRIDGE_DEFAULT_CWD", path.join(pkgRoot, "data", "scratch")),
	/** Event spool ring-buffer cap per session */
	spoolCap: Number(env("BRIDGE_SPOOL_CAP", "2000")),
	/** Truncation cap for large string fields inside spooled event payloads */
	eventFieldCap: Number(env("BRIDGE_EVENT_FIELD_CAP", String(16 * 1024))),
	/** Ms to wait for a host rpc ack before surfacing transport uncertainty */
	ackTimeoutMs: Number(env("BRIDGE_ACK_TIMEOUT_MS", "30000")),
	/** Ms to wait for a host to spawn + answer get_state */
	spawnTimeoutMs: Number(env("BRIDGE_SPAWN_TIMEOUT_MS", "90000")),
	/** Crash→respawn backoff. Tests raise this to open a deterministic
	 * host-down window (B4); production default is immediate respawn. */
	respawnDelayMs: Number(env("BRIDGE_RESPAWN_DELAY_MS", "0")),
	/** M8: pi-relay workspace_root analog — sessions/<id>/cwd subvolumes,
	 * workspace-bases/<project>/<dir>/, mcp-oauth-credentials.json. Must be a
	 * btrfs filesystem for subvolume semantics (probed at boot). The LIVE
	 * pi-relay base (~/.local/state/pi-relay) is never used by the bridge. */
	workspaceStateRoot: env("BRIDGE_WORKSPACE_STATE_ROOT", path.join(pkgRoot, "data", "workspace-state")),
	/** M8: fail boot when the workspace state root is not btrfs (default: fall
	 * back to plain dirs with a loud log — mirrors pi-relay's non-btrfs copy
	 * fallback; W1 exercises the real subvolume path). */
	workspaceRequireBtrfs: env("BRIDGE_WORKSPACE_REQUIRE_BTRFS", "0") === "1",
	/** M8: mcp.toml path (pi-relay ~/.config/pi-relay/runtime/mcp.toml analog). */
	mcpConfigPath: env("BRIDGE_MCP_CONFIG", path.join(pkgRoot, "data", "mcp.toml")),
	/** M8: loopback port for the OAuth callback listener (0 = ephemeral). */
	mcpOauthCallbackPort: Number(env("BRIDGE_MCP_OAUTH_CALLBACK_PORT", "0")),
	/** M8: sidecar title generation endpoint (GLM SSE shim chat/completions). */
	titleShimBaseUrl: env("BRIDGE_TITLE_BASE_URL", "http://127.0.0.1:8571/v1"),
	titleModel: env("BRIDGE_TITLE_MODEL", "nvidia/zai-org/glm-5.2"),
	titleApiKeyEnv: env("BRIDGE_TITLE_API_KEY_ENV", "NVIDIA_INFERENCE_API_KEY"),
	/** M10b: per-session host prompt-cache retention override, threaded to
	 * every session host as PI_CACHE_RETENTION (pi-ai resolveCacheRetention
	 * reads it per request: anthropic breakpoints get ttl:"1h" under "long";
	 * "none" disables cache_control). Null = untouched upstream default
	 * "short". */
	piCacheRetention: parseCacheRetention(env("BRIDGE_PI_CACHE_RETENTION", "")),
};


/** M8: the live pi-relay btrfs workspace base (~/.local/state/pi-relay) is
 * OFF-LIMITS (see .pi/migration-research/CONFIG-PATHS.md). Refuse to run if
 * the configured workspace state root resolves inside it — a symlinked or
 * misconfigured BRIDGE_WORKSPACE_STATE_ROOT must never touch live state. */
export function assertWorkspaceStateRootNotLiveBase(root: string): string {
	const liveBase = path.resolve(os.homedir(), ".local", "state", "pi-relay");
	const resolved = path.resolve(root);
	if (resolved === liveBase || resolved.startsWith(liveBase + path.sep)) {
		throw new Error(
			`BRIDGE_WORKSPACE_STATE_ROOT resolves inside the live pi-relay base (${liveBase}): ${resolved}. ` +
				"Refusing to start — live pi-relay state is off-limits.",
		);
	}
	return resolved;
}

export function sessionsDir(): string {
	return path.join(config.dataDir, "sessions");
}
export function journalDir(): string {
	return path.join(config.dataDir, "journal");
}
export function logsDir(): string {
	return path.join(config.dataDir, "logs");
}
