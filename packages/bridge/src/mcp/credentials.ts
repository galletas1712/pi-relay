// OAuth credential store — port of pi-relay rust/crates/agent-mcp/src/oauth_credentials.rs.
// Same file format (version 1), same credential key derivation, same bounds,
// same 0700-dir/0600-file atomic temp+rename writes.
//
// M8 EXTENSION: the file has TWO writers — the bridge (control plane) and the
// session kernels (mcp_base.py refresh). pi-relay's in-process Mutex is not
// enough; every read-modify-write takes a cross-process lock (`.lock`
// directory, spin with stale-break) and validates after re-read. Tokens never
// enter PG, logs, or any session workspace.
import { createHash } from "node:crypto";
import { existsSync, mkdirSync, readFileSync, renameSync, rmSync, chmodSync, statSync, writeFileSync, openSync, closeSync, constants } from "node:fs";
import { dirname, join } from "node:path";

export const FILE_VERSION = 1;
const MAX_FILE_BYTES = 1024 * 1024;
const MAX_IDENTITY_BYTES = 16 * 1024;
const MAX_TOKEN_BYTES = 256 * 1024;
const MAX_SCOPES = 256;
const MAX_SCOPE_BYTES = 4 * 1024;

export type CredentialStoreErrorCode =
	| "unavailable"
	| "io"
	| "empty"
	| "oversized"
	| "corrupt"
	| "unsupported_version"
	| "bounds"
	| "lock_timeout";

export class McpCredentialError extends Error {
	readonly code: CredentialStoreErrorCode;
	constructor(code: CredentialStoreErrorCode, message?: string) {
		super(message ?? `mcp_oauth_credential_store_failed: ${code}`);
		this.name = "McpCredentialError";
		this.code = code;
	}
}

export interface StoredOAuthCredential {
	server_id: string;
	server_url: string;
	configured_client_id?: string;
	resource?: string;
	client_id: string;
	access_token: string;
	refresh_token?: string;
	expires_at_millis?: number;
	granted_scopes: string[];
}

/** credential_key: sha256 of the compact JSON {"headers":{},"type":"http","url":...}
 * (serde_json Map is a BTreeMap → keys sorted), first 16 hex chars. */
export function credentialKey(serverId: string, serverUrl: string): string {
	const payload = `{"headers":{},"type":"http","url":${JSON.stringify(serverUrl)}}`;
	const digest = createHash("sha256").update(payload, "utf8").digest("hex");
	return `${serverId}|${digest.slice(0, 16)}`;
}

function validateCredential(c: StoredOAuthCredential): void {
	const identities = [c.server_id, c.server_url, c.client_id];
	if (
		identities.some((v) => typeof v !== "string" || v.trim() === "" || v.length > MAX_IDENTITY_BYTES) ||
		(c.configured_client_id !== undefined && (c.configured_client_id.trim() === "" || c.configured_client_id.length > MAX_IDENTITY_BYTES)) ||
		(c.resource !== undefined && c.resource.length > MAX_IDENTITY_BYTES) ||
		typeof c.access_token !== "string" ||
		c.access_token.trim() === "" ||
		c.access_token.length > MAX_TOKEN_BYTES ||
		(c.refresh_token !== undefined && (c.refresh_token.trim() === "" || c.refresh_token.length > MAX_TOKEN_BYTES)) ||
		!Array.isArray(c.granted_scopes) ||
		c.granted_scopes.length > MAX_SCOPES ||
		c.granted_scopes.some((s) => typeof s !== "string" || s.trim() === "" || s.length > MAX_SCOPE_BYTES)
	) {
		throw new McpCredentialError("bounds");
	}
}

interface CredentialFile {
	version: number;
	credentials: Record<string, StoredOAuthCredential>;
}

function validateFile(f: CredentialFile): void {
	if (f.version !== FILE_VERSION) throw new McpCredentialError("unsupported_version");
	for (const [key, cred] of Object.entries(f.credentials)) {
		validateCredential(cred);
		if (key !== credentialKey(cred.server_id, cred.server_url)) throw new McpCredentialError("corrupt");
	}
}

export function readCredentialFile(path: string): CredentialFile {
	let raw: string;
	try {
		const st = statSync(path);
		if (st.size > MAX_FILE_BYTES) throw new McpCredentialError("oversized");
		raw = readFileSync(path, "utf8");
	} catch (err) {
		if (err instanceof McpCredentialError) throw err;
		throw new McpCredentialError("io");
	}
	if (raw.trim() === "") throw new McpCredentialError("empty");
	if (new TextEncoder().encode(raw).length > MAX_FILE_BYTES) throw new McpCredentialError("oversized");
	let parsed: CredentialFile;
	try {
		parsed = JSON.parse(raw);
	} catch {
		throw new McpCredentialError("corrupt");
	}
	if (typeof parsed !== "object" || parsed === null || typeof parsed.credentials !== "object") throw new McpCredentialError("corrupt");
	validateFile(parsed);
	return parsed;
}

/** Atomic write: 0700 parent dir, 0600 temp file, fsync, rename (persist). */
export function writeCredentialFileAtomic(path: string, contents: CredentialFile): void {
	validateFile(contents);
	const parent = dirname(path);
	mkdirSync(parent, { recursive: true });
	try {
		chmodSync(parent, 0o700);
	} catch {
		/* best effort on non-posix */
	}
	const serialized = JSON.stringify(contents);
	if (new TextEncoder().encode(serialized).length > MAX_FILE_BYTES) throw new McpCredentialError("oversized");
	const tmp = join(parent, `.mcp-oauth-${process.pid}-${Math.random().toString(36).slice(2)}.tmp`);
	const fd = openSync(tmp, constants.O_WRONLY | constants.O_CREAT | constants.O_EXCL, 0o600);
	try {
		writeFileSync(fd, serialized);
	} finally {
		closeSync(fd);
	}
	try {
		chmodSync(tmp, 0o600);
	} catch {
		/* best effort */
	}
	renameSync(tmp, path);
}

/** Cross-process advisory lock: mkdir-based (atomic on POSIX), spin with a
 * stale-break for holders that died mid-write. Kernel mcp_base.py mirrors it. */
export async function withCredentialLock<T>(path: string, fn: () => T | Promise<T>, opts?: { timeoutMs?: number }): Promise<T> {
	const lockDir = `${path}.lock`;
	const timeoutMs = opts?.timeoutMs ?? 10_000;
	const t0 = Date.now();
	for (;;) {
		try {
			mkdirSync(lockDir);
			break;
		} catch {
			// stale-break: locks older than 60s belong to dead writers
			try {
				const st = statSync(lockDir);
				if (Date.now() - st.mtimeMs > 60_000) rmSync(lockDir, { recursive: true, force: true });
			} catch {
				/* raced */
			}
			if (Date.now() - t0 > timeoutMs) throw new McpCredentialError("lock_timeout");
			await new Promise((r) => setTimeout(r, 25));
		}
	}
	try {
		return await fn();
	} finally {
		rmSync(lockDir, { recursive: true, force: true });
	}
}

/** Repository: in-memory mirror + file persistence, mirroring pi-relay's
 * OAuthCredentialRepository but taking the cross-process lock for every
 * mutation and re-reading inside it (kernel may have refreshed). */
export class OAuthCredentialRepository {
	private readonly path: string;
	constructor(path: string) {
		this.path = path;
	}

	private load(): CredentialFile {
		if (!existsSync(this.path)) return { version: FILE_VERSION, credentials: {} };
		return readCredentialFile(this.path);
	}

	async get(serverId: string, serverUrl: string): Promise<StoredOAuthCredential | null> {
		const file = this.load();
		return file.credentials[credentialKey(serverId, serverUrl)] ?? null;
	}

	async save(credential: StoredOAuthCredential): Promise<void> {
		validateCredential(credential);
		await withCredentialLock(this.path, () => {
			const file = this.load();
			file.credentials[credentialKey(credential.server_id, credential.server_url)] = credential;
			writeCredentialFileAtomic(this.path, file);
		});
	}

	async remove(serverId: string, serverUrl: string): Promise<boolean> {
		return withCredentialLock(this.path, () => {
			const file = this.load();
			const key = credentialKey(serverId, serverUrl);
			if (!(key in file.credentials)) return false;
			delete file.credentials[key];
			writeCredentialFileAtomic(this.path, file);
			return true;
		});
	}

	async list(): Promise<StoredOAuthCredential[]> {
		return Object.values(this.load().credentials);
	}
}

/** Compatibility check (pi-relay is_compatible): a stored credential is
 * usable for this server config when identity + configured client id and
 * scopes line up. */
export function credentialCompatible(
	cred: StoredOAuthCredential,
	serverId: string,
	serverUrl: string,
	configuredClientId?: string,
	configuredScopes?: string[],
	resource?: string,
): boolean {
	return (
		cred.server_id === serverId &&
		cred.server_url === serverUrl &&
		cred.configured_client_id === configuredClientId &&
		cred.resource === resource &&
		(configuredClientId === undefined || configuredClientId === cred.client_id) &&
		(configuredScopes === undefined || configuredScopes.every((s) => cred.granted_scopes.includes(s)))
	);
}

export function credentialExpired(cred: StoredOAuthCredential, skewMs = 30_000): boolean {
	return cred.expires_at_millis !== undefined && Date.now() + skewMs >= cred.expires_at_millis;
}
