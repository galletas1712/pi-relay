import { defineConfig, loadEnv } from "vite";
import react from "@vitejs/plugin-react";

const DEFAULT_ALLOWED_HOSTS = ["odin.smelt-anaconda.ts.net"];

export default defineConfig(({ mode }) => {
	const env = loadEnv(mode, process.cwd(), "");
	const allowedHosts = Array.from(
		new Set([
			...DEFAULT_ALLOWED_HOSTS,
			...(env.VITE_PI_ALLOWED_HOSTS || "")
				.split(",")
				.map((host) => host.trim())
				.filter(Boolean)
		])
	);

	return {
		plugins: [react()],
		server: {
			host: "127.0.0.1",
			port: 8788,
			allowedHosts
		},
		preview: {
			allowedHosts
		}
	};
});
