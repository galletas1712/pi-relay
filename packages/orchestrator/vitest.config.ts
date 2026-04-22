import { resolve } from "node:path";
import { defineConfig } from "vitest/config";

export default defineConfig({
	resolve: {
		alias: {
			"@pi-relay/agent-protocol": resolve(__dirname, "../agent-protocol/src/index.ts"),
			"@pi-relay/orchestrator-core": resolve(__dirname, "../orchestrator-core/src/index.ts"),
		},
	},
	test: {
		environment: "node",
	},
});
