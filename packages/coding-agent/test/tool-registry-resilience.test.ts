/**
 * Verifies that a broken `ToolRegistry` resolve() (e.g. `configureTools`
 * pointing at an unknown provider) does NOT crash session start. The
 * ExtensionRunner wraps resolve() in a try/catch and emits a diagnostic
 * via the normal error channel.
 */

import { describe, expect, it } from "vitest";
import { AuthStorage } from "../src/core/auth-storage.js";
import { loadExtensions } from "../src/core/extensions/loader.js";
import { ExtensionRunner } from "../src/core/extensions/runner.js";
import type { ExtensionError } from "../src/core/extensions/types.js";
import { ModelRegistry } from "../src/core/model-registry.js";
import { SessionManager } from "../src/core/session-manager.js";

async function makeRunner(extPaths: string[], cwd: string): Promise<ExtensionRunner> {
	const result = await loadExtensions(extPaths, cwd);
	const sessionManager = SessionManager.inMemory();
	const modelRegistry = ModelRegistry.create(AuthStorage.inMemory());
	return new ExtensionRunner(result.extensions, result.runtime, cwd, sessionManager, modelRegistry);
}

describe("ExtensionRunner resilience to bad tool-registry config", () => {
	it("getAllRegisteredTools returns an empty list and emits a diagnostic when configureTools references an unknown provider", async () => {
		// Synthesize an extension that registers no providers but points
		// `tools.web_search` at an unknown provider id. The resolver throws
		// when it can't find the provider.
		const fs = await import("node:fs");
		const os = await import("node:os");
		const path = await import("node:path");

		const tempDir = fs.mkdtempSync(path.join(os.tmpdir(), "pi-runner-resilience-"));
		try {
			const extPath = path.join(tempDir, "broken-config.ts");
			fs.writeFileSync(
				extPath,
				`export default function (pi) {
  pi.configureTools({ web_search: { provider: "com.nonexistent.provider" } });
}
`,
				"utf-8",
			);

			const runner = await makeRunner([extPath], tempDir);
			const errors: ExtensionError[] = [];
			runner.onError((e) => errors.push(e));

			const tools = runner.getAllRegisteredTools();
			expect(tools).toEqual([]);
			expect(errors.length).toBeGreaterThanOrEqual(1);
			expect(errors[0].event).toBe("tool_registry_resolve");
			expect(errors[0].error).toMatch(/unknown provider "com\.nonexistent\.provider"/);

			// getToolDefinition swallows the same error.
			const def = runner.getToolDefinition("web_search");
			expect(def).toBeUndefined();
		} finally {
			fs.rmSync(tempDir, { recursive: true, force: true });
		}
	});
});
