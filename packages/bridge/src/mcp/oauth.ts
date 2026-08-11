// Bridge OAuth state machine for MCP servers — mirrors pi-relay's rmcp-based
// flow (rust/crates/agent-mcp/src/oauth_login.rs + oauth_runtime.rs) with the
// @modelcontextprotocol/sdk owning protocol mechanics (RFC 9728 protected
// resource metadata, RFC 8414/OIDC discovery, DCR, PKCE, token exchange,
// refresh). No parallel hand-rolled protocol code here.
//
// Flow (headless): mcp.login → auth() returns REDIRECT after invoking
// provider.redirectToAuthorization(url) → bridge returns {authorizationUrl,
// state} to the client (SPA opens a browser). A local loopback callback
// listener receives code+state → auth(..., authorizationCode) → tokens land
// in the 0600 credentials file via provider.saveTokens. Manual paste path:
// mcp.complete {server, code}. Tokens refresh through the same store
// (credential file is shared with session kernels, cross-process locked).
import { createHash, randomBytes } from "node:crypto";
import http from "node:http";
import { chmodSync, readFileSync, renameSync, writeFileSync } from "node:fs";
import type { AddressInfo } from "node:net";
import { auth, discoverOAuthMetadata, discoverOAuthProtectedResourceMetadata, refreshAuthorization, selectResourceURL, type OAuthClientProvider } from "@modelcontextprotocol/sdk/client/auth.js";
import type { OAuthClientMetadata, OAuthTokens } from "@modelcontextprotocol/sdk/shared/auth.js";
import { credentialCompatible, credentialExpired, OAuthCredentialRepository, type StoredOAuthCredential } from "./credentials.ts";
import type { McpOAuthConfig } from "./config.ts";

export type McpAuthStatus =
	| "non_oauth"
	| "unsupported"
	| "unknown"
	| "bearer"
	| "login_required"
	| "reauthentication_required"
	| "oauth_ready"
	| "authorization_pending";

export class McpOAuthError extends Error {
	constructor(message: string) {
		super(message);
		this.name = "McpOAuthError";
	}
}

const CLIENT_NAME = "pi-relay-bridge";

/** DCR results are persisted next to the credentials (0600) so a server is
 * registered once per redirect URL, not once per login. */
interface ClientRegistrationFile {
	version: 1;
	clients: Record<string, { client_id: string; client_secret?: string }>;
}

function sha256Hex(s: string): string {
	return createHash("sha256").update(s, "utf8").digest("hex");
}

class BridgeOAuthProvider implements OAuthClientProvider {
	codeVerifierValue = "";
	authorizationUrl: URL | null = null;
	savedTokens: OAuthTokens | null = null;
	savedClientInfo: { client_id: string; client_secret?: string } | null = null;
	readonly stateValue = randomBytes(16).toString("hex");

	readonly redirectUrl: string;
	readonly serverId: string;
	readonly serverUrl: string;
	readonly oauth: McpOAuthConfig;
	private readonly registration: { client_id: string; client_secret?: string } | null;

	constructor(
		redirectUrl: string,
		serverId: string,
		serverUrl: string,
		oauth: McpOAuthConfig,
		registration: { client_id: string; client_secret?: string } | null,
	) {
		this.redirectUrl = redirectUrl;
		this.serverId = serverId;
		this.serverUrl = serverUrl;
		this.oauth = oauth;
		this.registration = registration;
	}

	get clientMetadata(): OAuthClientMetadata {
		return {
			client_name: CLIENT_NAME,
			redirect_uris: [this.redirectUrl],
			grant_types: ["authorization_code", "refresh_token"],
			response_types: ["code"],
			token_endpoint_auth_method: "none",
			scope: this.oauth.scopes?.join(" "),
		};
	}

	state(): string {
		return this.stateValue;
	}

	clientInformation() {
		if (this.oauth.clientId) {
			return { client_id: this.oauth.clientId, ...(this.registration?.client_secret ? { client_secret: this.registration.client_secret } : {}) };
		}
		// DCR during THIS login persists into savedClientInfo; the exchange leg
		// (a second auth() call on the same provider) needs it back.
		return this.savedClientInfo ?? this.registration ?? undefined;
	}

	saveClientInformation(info: { client_id: string; client_secret?: string }): void {
		this.savedClientInfo = { client_id: info.client_id, ...(info.client_secret ? { client_secret: info.client_secret } : {}) };
	}

	tokens(): OAuthTokens | undefined {
		return undefined; // logins always start unauthenticated; refresh goes through the credential store
	}

	saveTokens(tokens: OAuthTokens): void {
		this.savedTokens = tokens;
	}

	redirectToAuthorization(authorizationUrl: URL): void {
		this.authorizationUrl = authorizationUrl;
	}

	saveCodeVerifier(codeVerifier: string): void {
		this.codeVerifierValue = codeVerifier;
	}

	codeVerifier(): string {
		return this.codeVerifierValue;
	}
}

export interface PendingLogin {
	serverId: string;
	state: string;
	authorizationUrl: string;
	callbackPort: number;
	expiresAtMs: number;
}

interface PendingInternal {
	provider: BridgeOAuthProvider;
	server: http.Server;
	resolve: (cred: StoredOAuthCredential) => void;
	reject: (err: Error) => void;
	timeout: NodeJS.Timeout;
	done: Promise<StoredOAuthCredential>;
}

export interface OAuthManagerOptions {
	credentials: OAuthCredentialRepository;
	defaultCallbackPort?: number; // config.mcpOauthCallbackPort; 0/undefined = ephemeral
	registrationPath?: string; // 0600 sidecar for DCR results
}

export class OAuthManager {
	private readonly pending = new Map<string, PendingInternal>();

	private readonly opts: OAuthManagerOptions;
	constructor(opts: OAuthManagerOptions) {
		this.opts = opts;
	}

	private registrationKey(serverId: string, redirectUrl: string): string {
		return `${serverId}|${sha256Hex(redirectUrl).slice(0, 16)}`;
	}

	private loadRegistrations(): ClientRegistrationFile {
		const path = this.opts.registrationPath;
		if (!path) return { version: 1, clients: {} };
		try {
			const raw = readFileSync(path, "utf8");
			const parsed = JSON.parse(raw);
			if (parsed?.version === 1 && typeof parsed.clients === "object") return parsed;
		} catch {
			/* absent/corrupt → fresh */
		}
		return { version: 1, clients: {} };
	}

	private saveRegistration(serverId: string, redirectUrl: string, info: { client_id: string; client_secret?: string }): void {
		const path = this.opts.registrationPath;
		if (!path) return;
		const file = this.loadRegistrations();
		file.clients[this.registrationKey(serverId, redirectUrl)] = info;
		const tmp = `${path}.tmp-${process.pid}`;
		writeFileSync(tmp, JSON.stringify(file), { mode: 0o600 });
		renameSync(tmp, path);
		try {
			chmodSync(path, 0o600);
		} catch {
			/* best effort */
		}
	}

	/** Stage 1-4: discovery → DCR → PKCE → authorization URL. Returns the URL
	 * the user must visit; completion arrives via the loopback callback or
	 * completeLogin. */
	async beginLogin(serverId: string, serverUrl: string, oauth: McpOAuthConfig): Promise<PendingLogin> {
		if (this.pending.has(serverId)) throw new McpOAuthError(`mcp oauth login already pending for ${serverId}`);
		const callbackPort = oauth.callbackPort ?? this.opts.defaultCallbackPort ?? 0;

		const server = http.createServer();
		await new Promise<void>((resolve, reject) => {
			server.once("error", reject);
			server.listen(callbackPort, "127.0.0.1", () => resolve());
		});
		const port = (server.address() as AddressInfo).port;
		const redirectUrl = `http://127.0.0.1:${port}/oauth/callback`;

		const registration = this.loadRegistrations().clients[this.registrationKey(serverId, redirectUrl)] ?? null;
		const provider = new BridgeOAuthProvider(redirectUrl, serverId, serverUrl, oauth, registration);

		const internal: PendingInternal = {
			provider,
			server,
			resolve: () => {},
			reject: () => {},
			timeout: setTimeout(() => {}, 0),
			done: Promise.resolve(null as never),
		};
		internal.done = new Promise<StoredOAuthCredential>((resolve, reject) => {
			internal.resolve = resolve;
			internal.reject = reject;
		});
		internal.done.catch(() => {}); // completion handlers attach via waitLogin; avoid unhandled rejection
		internal.timeout = setTimeout(() => {
			this.failPending(serverId, new McpOAuthError(`mcp oauth callback timeout for ${serverId}`));
		}, oauth.callbackTimeoutMs);
		this.pending.set(serverId, internal);

		server.on("request", (req, res) => {
			void this.handleCallback(serverId, req, res);
		});

		try {
			const result = await auth(provider, {
				serverUrl,
				scope: oauth.scopes?.join(" "),
			});
			if (result === "AUTHORIZED") {
				// already-authorized path should not happen (tokens() is always undefined)
				throw new McpOAuthError(`mcp oauth unexpected immediate authorization for ${serverId}`);
			}
		} catch (err) {
			this.pending.delete(serverId);
			clearTimeout(internal.timeout);
			server.close();
			throw err instanceof Error ? err : new McpOAuthError(String(err));
		}
		if (provider.savedClientInfo) this.saveRegistration(serverId, redirectUrl, provider.savedClientInfo);
		if (!provider.authorizationUrl) {
			this.pending.delete(serverId);
			clearTimeout(internal.timeout);
			server.close();
			throw new McpOAuthError(`mcp oauth did not produce an authorization URL for ${serverId}`);
		}
		return {
			serverId,
			state: provider.stateValue,
			authorizationUrl: provider.authorizationUrl.toString(),
			callbackPort: port,
			expiresAtMs: Date.now() + oauth.callbackTimeoutMs,
		};
	}

	private async handleCallback(serverId: string, req: http.IncomingMessage, res: http.ServerResponse): Promise<void> {
		const pending = this.pending.get(serverId);
		const url = new URL(req.url ?? "/", "http://127.0.0.1");
		const finish = (status: number, body: string) => {
			res.writeHead(status, { "content-type": "text/html; charset=utf-8" });
			res.end(body);
		};
		if (url.pathname !== "/oauth/callback") {
			finish(404, "not found");
			return;
		}
		if (!pending) {
			finish(409, "no login pending for this server");
			return;
		}
		const errParam = url.searchParams.get("error");
		if (errParam) {
			const msg = `mcp oauth authorization failed: ${errParam}`;
			finish(400, msg);
			this.failPending(serverId, new McpOAuthError(msg));
			return;
		}
		const code = url.searchParams.get("code");
		const state = url.searchParams.get("state");
		if (!code || state !== pending.provider.stateValue) {
			finish(400, "invalid oauth callback (state mismatch)");
			return;
		}
		try {
			const cred = await this.exchange(serverId, pending.provider, code);
			finish(200, "<html><body><h3>pi-relay MCP login complete</h3>You can close this tab.</body></html>");
			this.completePending(serverId, cred);
		} catch (err) {
			finish(500, "token exchange failed");
			this.failPending(serverId, err instanceof Error ? err : new McpOAuthError(String(err)));
		}
	}

	private async exchange(serverId: string, provider: BridgeOAuthProvider, code: string): Promise<StoredOAuthCredential> {
		const result = await auth(provider, { serverUrl: provider.serverUrl, authorizationCode: code });
		if (result !== "AUTHORIZED" || !provider.savedTokens) throw new McpOAuthError(`mcp oauth token exchange failed for ${serverId}`);
		if (provider.savedClientInfo) this.saveRegistration(serverId, provider.redirectUrl, provider.savedClientInfo);
		return this.persistTokens(serverId, provider.serverUrl, provider.oauth, provider.savedTokens, provider.savedClientInfo?.client_id);
	}

	private async persistTokens(serverId: string, serverUrl: string, oauth: McpOAuthConfig, tokens: OAuthTokens, registeredClientId?: string): Promise<StoredOAuthCredential> {
		const prior = await this.opts.credentials.get(serverId, serverUrl);
		const clientId = oauth.clientId ?? registeredClientId ?? prior?.client_id;
		if (!clientId) throw new McpOAuthError(`mcp oauth missing client id for ${serverId}`);
		const cred: StoredOAuthCredential = {
			server_id: serverId,
			server_url: serverUrl,
			configured_client_id: oauth.clientId,
			resource: oauth.resource,
			client_id: clientId,
			access_token: tokens.access_token,
			refresh_token: tokens.refresh_token ?? prior?.refresh_token,
			expires_at_millis: tokens.expires_in !== undefined ? Date.now() + tokens.expires_in * 1000 : undefined,
			granted_scopes: tokens.scope ? tokens.scope.split(" ").filter(Boolean) : (prior?.granted_scopes ?? oauth.scopes ?? []),
		};
		await this.opts.credentials.save(cred);
		return cred;
	}

	private completePending(serverId: string, cred: StoredOAuthCredential): void {
		const pending = this.pending.get(serverId);
		if (!pending) return;
		this.pending.delete(serverId);
		clearTimeout(pending.timeout);
		pending.server.close();
		pending.resolve(cred);
	}

	private failPending(serverId: string, err: Error): void {
		const pending = this.pending.get(serverId);
		if (!pending) return;
		this.pending.delete(serverId);
		clearTimeout(pending.timeout);
		pending.server.close();
		pending.reject(err);
	}

	/** Manual-paste path (mcp.complete): the user copied the code out of the
	 * browser URL bar. */
	async completeLogin(serverId: string, code: string, state?: string): Promise<StoredOAuthCredential> {
		const pending = this.pending.get(serverId);
		if (!pending) throw new McpOAuthError(`no mcp oauth login pending for ${serverId}`);
		if (state !== undefined && state !== pending.provider.stateValue) throw new McpOAuthError("oauth state mismatch");
		const cred = await this.exchange(serverId, pending.provider, code);
		this.completePending(serverId, cred);
		return cred;
	}

	/** Wait for the loopback callback to complete a pending login. */
	async waitLogin(serverId: string): Promise<StoredOAuthCredential> {
		const pending = this.pending.get(serverId);
		if (!pending) throw new McpOAuthError(`no mcp oauth login pending for ${serverId}`);
		return pending.done;
	}

	cancelLogin(serverId: string): boolean {
		const had = this.pending.has(serverId);
		if (had) this.failPending(serverId, new McpOAuthError(`mcp oauth login cancelled for ${serverId}`));
		return had;
	}

	pendingInfo(serverId: string): PendingLogin | null {
		const p = this.pending.get(serverId);
		if (!p) return null;
		return {
			serverId,
			state: p.provider.stateValue,
			authorizationUrl: p.provider.authorizationUrl?.toString() ?? "",
			callbackPort: (p.server.address() as AddressInfo)?.port ?? 0,
			expiresAtMs: 0,
		};
	}

	/** Refresh an expired credential via its refresh_token (SDK
	 * refreshAuthorization after discovery). Persists rotated tokens. */
	async refresh(serverId: string, serverUrl: string, oauth: McpOAuthConfig): Promise<StoredOAuthCredential> {
		const cred = await this.opts.credentials.get(serverId, serverUrl);
		if (!cred) throw new McpOAuthError(`no stored mcp oauth credential for ${serverId}`);
		if (!credentialExpired(cred)) return cred;
		if (!cred.refresh_token) throw new McpOAuthError(`mcp oauth credential expired without refresh token for ${serverId}`);

		// discovery: RFC 9728 → RFC 8414 (SDK helpers; 404 tolerated)
		let asUrl: string | URL = serverUrl;
		let metadata;
		try {
			const resourceMetadata = await discoverOAuthProtectedResourceMetadata(serverUrl);
			const resource = await selectResourceURL(serverUrl, { resourceMetadata: () => undefined } as unknown as OAuthClientProvider, resourceMetadata);
			const asFromResource = resourceMetadata?.authorization_servers?.[0];
			if (asFromResource) asUrl = asFromResource;
			metadata = await discoverOAuthMetadata(asUrl);
			void resource;
		} catch (err) {
			throw new McpOAuthError(`mcp oauth discovery failed for ${serverId}: ${err instanceof Error ? err.message : err}`);
		}
		if (!metadata) throw new McpOAuthError(`mcp oauth discovery failed for ${serverId}`);

		const tokens = await refreshAuthorization(asUrl, {
			metadata,
			clientInformation: { client_id: cred.client_id },
			refreshToken: cred.refresh_token,
			resource: oauth.resource ? new URL(oauth.resource) : undefined,
		});
		return this.persistTokens(serverId, serverUrl, oauth, tokens);
	}

	/** Auth status for inventory/status surfaces. Never exposes token bytes. */
	async status(serverId: string, serverUrl: string, oauth: McpOAuthConfig | null, hasBearer: boolean): Promise<McpAuthStatus> {
		if (oauth === null) return hasBearer ? "bearer" : "non_oauth";
		if (this.pending.has(serverId)) return "authorization_pending";
		const cred = await this.opts.credentials.get(serverId, serverUrl);
		if (!cred) return "login_required";
		if (!credentialCompatible(cred, serverId, serverUrl, oauth.clientId, oauth.scopes, oauth.resource)) return "login_required";
		if (credentialExpired(cred)) return cred.refresh_token ? "reauthentication_required" : "login_required";
		return "oauth_ready";
	}

	async logout(serverId: string, serverUrl: string): Promise<boolean> {
		this.cancelLogin(serverId);
		return this.opts.credentials.remove(serverId, serverUrl);
	}
}
