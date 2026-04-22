import { resolve } from "node:path";
import { defineConfig } from "vitest/config";

export default defineConfig({
	resolve: {
		alias: {
			"@pi-relay/agent-protocol": resolve(__dirname, "../agent-protocol/src/index.ts"),
		},
	},
	test: {
		environment: "node",
	},
});
