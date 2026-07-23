import path from "node:path";
import { fileURLToPath } from "node:url";
import tailwindcss from "@tailwindcss/vite";
import react from "@vitejs/plugin-react";
import { defineConfig, loadEnv } from "vite";

const rootDir = path.dirname(fileURLToPath(import.meta.url));

// Host validation is opt-in. Deployment/tailnet hosts must be supplied via
// VITE_PI_ALLOWED_HOSTS rather than being embedded in the repository.
const DEFAULT_ALLOWED_HOSTS: string[] = [];

export default defineConfig(({ mode }) => {
	const env = loadEnv(mode, process.cwd(), "");
	const allowedHosts = Array.from(
		new Set([
			...DEFAULT_ALLOWED_HOSTS,
			...(env.VITE_PI_ALLOWED_HOSTS || "")
				.split(",")
				.map((host) => host.trim())
				.filter(Boolean),
		]),
	);

	return {
		plugins: [react(), tailwindcss()],
		resolve: {
			alias: {
				"@": path.resolve(rootDir, "./src"),
			},
		},
		server: {
			host: "127.0.0.1",
			port: 8788,
			allowedHosts,
		},
		preview: {
			allowedHosts,
		},
	};
});
