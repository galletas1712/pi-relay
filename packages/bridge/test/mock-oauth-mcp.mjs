// Mock OAuth authorization server + MCP streamable-HTTP server for M8 tests.
// Implements the slices of RFC 9728/8414/OIDC/DCR/PKCE that the
// @modelcontextprotocol/sdk client drives: well-known discovery (OAuth AS +
// OIDC + protected-resource), dynamic client registration, authorization-code
// flow with auto-approve redirect (mock_deny=1 to deny), token exchange with
// S256 PKCE verification, refresh rotation, and a 401+WWW-Authenticate MCP
// endpoint exposing mock.echo / mock.time.
import http from "node:http";
import { createHash, randomBytes } from "node:crypto";

function base64url(buf) {
	return Buffer.from(buf).toString("base64url");
}

export function startMockOAuthMcp(opts = {}) {
	const {
		requireOAuth = true,
		staticBearer = null, // when set, this token is also accepted (bearer_env path)
		tokenTtlSeconds = 3600,
		autoApprove = true,
		tools = null,
	} = opts;
	const codes = new Map(); // code → {challenge, clientId, scope, redirectUri}
	const tokens = new Map(); // access → {expiresAt, scope, clientId}
	const refreshTokens = new Map(); // refresh → {clientId, scope}
	const grants = { register: 0, authorize: 0, token: 0, refresh: 0, denied: 0, mcp401: 0, pkceFails: 0 };

	const defaultTools = [
		{ name: "mock.echo", description: "Echo back the text argument", inputSchema: { type: "object", properties: { text: { type: "string" } }, required: ["text"] } },
		{ name: "mock.time", description: "Return the mock server time", inputSchema: { type: "object", properties: {} } },
	];
	const exposedTools = tools ?? defaultTools;

	function issueToken(clientId, scope) {
		const access = "mock-at-" + randomBytes(12).toString("hex");
		const refresh = "mock-rt-" + randomBytes(12).toString("hex");
		tokens.set(access, { expiresAt: Date.now() + tokenTtlSeconds * 1000, scope, clientId });
		refreshTokens.set(refresh, { clientId, scope });
		return { access_token: access, refresh_token: refresh, expires_in: tokenTtlSeconds, token_type: "Bearer", ...(scope ? { scope } : {}) };
	}

	function authed(req, origin) {
		if (staticBearer) {
			const h = req.headers.authorization;
			return h === `Bearer ${staticBearer}`;
		}
		if (!requireOAuth) return true;
		const h = req.headers.authorization ?? "";
		const token = h.startsWith("Bearer ") ? h.slice(7) : "";
		const entry = tokens.get(token);
		return !!entry && entry.expiresAt > Date.now();
	}

	function json(res, status, body, headers = {}) {
		res.writeHead(status, { "content-type": "application/json", ...headers });
		res.end(JSON.stringify(body));
	}

	const server = http.createServer((req, res) => {
		void (async () => {
			const url = new URL(req.url ?? "/", "http://127.0.0.1");
			const origin = `http://127.0.0.1:${server.address().port}`;
			const chunks = [];
			for await (const c of req) chunks.push(c);
			const body = Buffer.concat(chunks).toString("utf8");

			if (url.pathname === "/.well-known/oauth-protected-resource" || url.pathname.startsWith("/.well-known/oauth-protected-resource/")) {
				return json(res, 200, { resource: `${origin}/mcp`, authorization_servers: [origin], scopes_supported: ["mcp:read"] });
			}
			if (
				url.pathname === "/.well-known/oauth-authorization-server" ||
				url.pathname === "/.well-known/openid-configuration" ||
				url.pathname.startsWith("/.well-known/oauth-authorization-server/") ||
				url.pathname.startsWith("/.well-known/openid-configuration/")
			) {
				return json(res, 200, {
					issuer: origin,
					authorization_endpoint: `${origin}/authorize`,
					token_endpoint: `${origin}/token`,
					registration_endpoint: `${origin}/register`,
					response_types_supported: ["code"],
					grant_types_supported: ["authorization_code", "refresh_token"],
					code_challenge_methods_supported: ["S256"],
					token_endpoint_auth_methods_supported: ["none", "client_secret_basic"],
				});
			}
			if (url.pathname === "/register" && req.method === "POST") {
				grants.register++;
				const meta = body ? JSON.parse(body) : {};
				return json(res, 201, { ...meta, client_id: `mock-client-${randomBytes(6).toString("hex")}`, token_endpoint_auth_method: "none" });
			}
			if (url.pathname === "/authorize" && req.method === "GET") {
				grants.authorize++;
				const redirectUri = url.searchParams.get("redirect_uri");
				const state = url.searchParams.get("state") ?? "";
				const challenge = url.searchParams.get("code_challenge");
				const method = url.searchParams.get("code_challenge_method");
				if (!redirectUri || method !== "S256" || !challenge) return json(res, 400, { error: "invalid_request" });
				const target = new URL(redirectUri);
				if (!autoApprove || url.searchParams.get("mock_deny") === "1") {
					grants.denied++;
					target.searchParams.set("error", "access_denied");
					if (state) target.searchParams.set("state", state);
					res.writeHead(302, { location: target.toString() });
					return res.end();
				}
				const code = `mock-code-${randomBytes(8).toString("hex")}`;
				codes.set(code, {
					challenge,
					clientId: url.searchParams.get("client_id"),
					scope: url.searchParams.get("scope") ?? "",
					redirectUri,
				});
				target.searchParams.set("code", code);
				if (state) target.searchParams.set("state", state);
				res.writeHead(302, { location: target.toString() });
				return res.end();
			}
			if (url.pathname === "/token" && req.method === "POST") {
				grants.token++;
				const form = new URLSearchParams(body);
				const grantType = form.get("grant_type");
				if (grantType === "authorization_code") {
					const code = form.get("code");
					const entry = codes.get(code);
					if (!entry) return json(res, 400, { error: "invalid_grant" });
					codes.delete(code);
					const verifier = form.get("code_verifier") ?? "";
					if (base64url(createHash("sha256").update(verifier).digest()) !== entry.challenge) {
						grants.pkceFails++;
						return json(res, 400, { error: "invalid_grant", error_description: "pkce verification failed" });
					}
					return json(res, 200, issueToken(entry.clientId, entry.scope));
				}
				if (grantType === "refresh_token") {
					grants.refresh++;
					const rt = form.get("refresh_token");
					const entry = refreshTokens.get(rt);
					if (!entry) return json(res, 400, { error: "invalid_grant" });
					refreshTokens.delete(rt); // rotate
					return json(res, 200, issueToken(entry.clientId, entry.scope));
				}
				return json(res, 400, { error: "unsupported_grant_type" });
			}
			if (url.pathname === "/mcp") {
				if (!authed(req, origin)) {
					grants.mcp401++;
					res.writeHead(401, {
						"content-type": "application/json",
						"www-authenticate": `Bearer realm="mcp", resource_metadata="${origin}/.well-known/oauth-protected-resource"`,
					});
					return res.end(JSON.stringify({ error: "unauthorized" }));
				}
				let msg;
				try {
					msg = JSON.parse(body);
				} catch {
					return json(res, 400, { error: "bad json" });
				}
				if (msg.method === "initialize") {
					return json(res, 200, {
						jsonrpc: "2.0",
						id: msg.id,
						result: {
							protocolVersion: msg.params?.protocolVersion ?? "2025-03-26",
							capabilities: { tools: {} },
							serverInfo: { name: "mock-mcp", version: "1.0.0" },
						},
					});
				}
				if (String(msg.method ?? "").startsWith("notifications/")) {
					res.writeHead(202);
					return res.end();
				}
				if (msg.method === "tools/list") {
					return json(res, 200, { jsonrpc: "2.0", id: msg.id, result: { tools: exposedTools } });
				}
				if (msg.method === "tools/call") {
					const { name, arguments: args } = msg.params ?? {};
					if (name === "mock.echo") {
						return json(res, 200, { jsonrpc: "2.0", id: msg.id, result: { content: [{ type: "text", text: String(args?.text ?? "") }], isError: false } });
					}
					if (name === "mock.time") {
						return json(res, 200, { jsonrpc: "2.0", id: msg.id, result: { content: [{ type: "text", text: "MOCK-TIME-1234" }], isError: false } });
					}
					return json(res, 200, { jsonrpc: "2.0", id: msg.id, result: { content: [{ type: "text", text: `unknown tool ${name}` }], isError: true } });
				}
				return json(res, 200, { jsonrpc: "2.0", id: msg.id ?? null, error: { code: -32601, message: "method not found" } });
			}
			json(res, 404, { error: "not found" });
		})().catch((err) => json(res, 500, { error: String(err) }));
	});

	return new Promise((resolve) => {
		server.listen(0, "127.0.0.1", () => {
			resolve({
				server,
				port: server.address().port,
				url: `http://127.0.0.1:${server.address().port}/mcp`,
				origin: `http://127.0.0.1:${server.address().port}`,
				grants,
				close: () => new Promise((r) => server.close(r)),
			});
		});
	});
}
