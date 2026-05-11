import { defineConfig, loadEnv } from "vite";
import react from "@vitejs/plugin-react";

export default defineConfig(({ mode }) => {
	const env = loadEnv(mode, process.cwd(), "");
	const allowedHosts = (env.VITE_PI_ALLOWED_HOSTS || "")
		.split(",")
		.map((host) => host.trim())
		.filter(Boolean);

	return {
		plugins: [react()],
		server: {
			host: "127.0.0.1",
			port: 8788,
			allowedHosts: allowedHosts.length > 0 ? allowedHosts : undefined
		}
	};
});
