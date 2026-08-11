import path from "node:path";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import tailwindcss from "@tailwindcss/vite";
import react from "@vitejs/plugin-react";
import { defineConfig } from "vite";

const rootDir = path.dirname(fileURLToPath(import.meta.url));

// Bridge-profile dev proxy: browsers cannot set Authorization/Origin headers
// on WebSocket upgrades, so the bridge-profile app connects same-origin to
// /__bridge-ws and the dev server proxies to the bridge with the upgrade
// headers injected. The token is read server-side only and never shipped to
// the bundle. Inert unless something connects to /__bridge-ws (only the
// bridge profile does).
function bridgeProxyHeaders(): Record<string, string> | null {
	const tokenFile =
		process.env.BRIDGE_TOKEN_FILE ?? path.resolve(rootDir, "../../.pi/m1-demo/.bridge-auth-token");
	try {
		const token = readFileSync(tokenFile, "utf8").replace(/[\r\n]/g, "");
		// must be an origin the bridge allowlists (BRIDGE_ALLOWED_ORIGINS)
		return { authorization: `Bearer ${token}`, origin: process.env.BRIDGE_PROXY_ORIGIN ?? "http://localhost:3000" };
	} catch {
		return null;
	}
}

const bridgeHeaders = bridgeProxyHeaders();
const bridgePort = process.env.BRIDGE_PORT ?? "8730";

export default defineConfig({
	plugins: [react(), tailwindcss()],
	resolve: {
		alias: {
			"@": path.resolve(rootDir, "./src"),
		},
	},
	optimizeDeps: {
		include: ["@pierre/diffs", "@pierre/diffs/react"],
	},
	server: {
		host: "127.0.0.1",
		port: 8788,
		proxy: bridgeHeaders
			? {
					"/__bridge-ws": {
						target: `ws://127.0.0.1:${bridgePort}`,
						ws: true,
						changeOrigin: false,
						headers: bridgeHeaders,
					},
				}
			: undefined,
	},
});
