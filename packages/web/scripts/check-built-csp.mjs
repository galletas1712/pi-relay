import { readFile } from "node:fs/promises";

const html = await readFile(new URL("../dist/index.html", import.meta.url), "utf8");
const headers = await readFile(new URL("../dist/_headers", import.meta.url), "utf8");
const scripts = [...html.matchAll(/<script\b([^>]*)>([\s\S]*?)<\/script>/giu)];

const routes = new Map();
let route;
for (const line of headers.split(/\r?\n/u)) {
	if (!line.trim() || line.trimStart().startsWith("#")) continue;

	if (!/^\s/u.test(line)) {
		route = line.trim();
		if (routes.has(route)) {
			throw new Error(`shipped Cloudflare Pages _headers repeats ${route}`);
		}
		routes.set(route, new Map());
		continue;
	}

	if (route === undefined) {
		throw new Error("shipped Cloudflare Pages _headers must begin with a route");
	}
	const separator = line.indexOf(":");
	if (separator === -1) {
		throw new Error("shipped Cloudflare Pages _headers contains a malformed header");
	}
	const name = line.slice(0, separator).trim().toLowerCase();
	const policyHeaders = routes.get(route);
	if (policyHeaders.has(name)) {
		throw new Error(`shipped Cloudflare Pages _headers repeats ${name} for ${route}`);
	}
	policyHeaders.set(name, line.slice(separator + 1).trim());
}

if (!routes.has("/*") || routes.size !== 2 || !routes.has("/service-worker.js")) {
	throw new Error("shipped Cloudflare Pages _headers must have /* and /service-worker.js routes");
}

const policyHeaders = routes.get("/*");
const cspDirectives = new Map();
for (const directive of (policyHeaders.get("content-security-policy") ?? "").split(";")) {
	const [name, ...values] = directive.trim().split(/\s+/u);
	if (!name) continue;
	if (cspDirectives.has(name)) {
		throw new Error(`shipped Cloudflare Pages CSP repeats ${name}`);
	}
	cspDirectives.set(name, values);
}

function requireDirective(name, expected) {
	const actual = cspDirectives.get(name);
	if (
		actual?.length !== expected.length ||
		expected.some((value, index) => actual[index] !== value)
	) {
		throw new Error(`shipped Cloudflare Pages CSP must set ${name} to ${expected.join(" ")}`);
	}
}

requireDirective("script-src", ["'self'"]);
requireDirective("object-src", ["'none'"]);
requireDirective("base-uri", ["'none'"]);
requireDirective("frame-ancestors", ["'none'"]);
requireDirective("connect-src", [
	"'self'",
	"wss:",
	"ws://127.0.0.1:*",
	"ws://localhost:*",
	"ws://[::1]:*",
]);

if (policyHeaders.get("referrer-policy") !== "no-referrer") {
	throw new Error("shipped Cloudflare Pages policy must suppress referrers");
}
if (policyHeaders.get("x-content-type-options") !== "nosniff") {
	throw new Error("shipped Cloudflare Pages policy must disable content sniffing");
}
if (routes.get("/service-worker.js").get("cache-control") !== "no-cache") {
	throw new Error("shipped Cloudflare Pages service worker must be revalidated promptly");
}
if (
	scripts.length === 0 ||
	scripts.some((match) => !/\bsrc\s*=/iu.test(match[1]) || match[2].trim())
) {
	throw new Error("production HTML must contain only external scripts under the shipped CSP");
}
