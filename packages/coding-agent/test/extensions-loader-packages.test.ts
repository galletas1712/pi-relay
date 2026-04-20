/**
 * Tests for loadExtensions / resolveExtensionEntry support of:
 *   - bare string package names,
 *   - object forms `{ path }` and `{ package }`,
 *   - nonexistent-package diagnostics (no crash).
 *
 * Sets up a fake package under a temp `node_modules/` so Node's
 * `import.meta.resolve` can find it without polluting the real workspace.
 */

import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { loadExtensions, resolveExtensionEntry } from "../src/core/extensions/loader.js";

const FAKE_PACKAGE_SRC = `
import { defineToolProvider } from "@pi-relay/tool-kit";
import { Type } from "@sinclair/typebox";

const provider = defineToolProvider({
  id: "com.test.pkg-ext",
  implements: "web_search",
  displayName: "Pkg Ext",
  version: "0.0.1",
  parameters: Type.Object({ query: Type.String() }),
  async execute(params, ctx) {
    return {
      content: [{ type: "text", text: "pkg:" + params.query + ":" + ctx.toolName }],
      details: { toolName: ctx.toolName },
    };
  },
});

export default function (pi) {
  pi.registerToolProvider(provider);
}
`;

describe("extension loader package-name resolution", () => {
	let tempDir: string;
	let pkgDir: string;

	beforeEach(() => {
		tempDir = fs.mkdtempSync(path.join(os.tmpdir(), "pi-pkg-ext-"));
		pkgDir = path.join(tempDir, "node_modules", "@pi-relay-test", "fake-pack");
		fs.mkdirSync(pkgDir, { recursive: true });
		fs.writeFileSync(
			path.join(pkgDir, "package.json"),
			JSON.stringify(
				{
					name: "@pi-relay-test/fake-pack",
					version: "0.0.1",
					type: "module",
					main: "./index.ts",
					exports: {
						".": {
							import: "./index.ts",
						},
					},
				},
				null,
				2,
			),
			"utf-8",
		);
		fs.writeFileSync(path.join(pkgDir, "index.ts"), FAKE_PACKAGE_SRC, "utf-8");
	});

	afterEach(() => {
		fs.rmSync(tempDir, { recursive: true, force: true });
	});

	it("resolveExtensionEntry classifies entries as file vs package", () => {
		const pkgResolved = resolveExtensionEntry("@pi-relay-test/fake-pack", tempDir);
		expect("error" in pkgResolved).toBe(false);
		if ("error" in pkgResolved) return;
		expect(pkgResolved.kind).toBe("package");
		expect(pkgResolved.resolvedPath.endsWith("index.ts")).toBe(true);

		const relResolved = resolveExtensionEntry("./foo.ts", tempDir);
		expect("error" in relResolved).toBe(false);
		if ("error" in relResolved) return;
		expect(relResolved.kind).toBe("file");

		const absResolved = resolveExtensionEntry("/abs/path.ts", tempDir);
		expect("error" in absResolved).toBe(false);
		if ("error" in absResolved) return;
		expect(absResolved.kind).toBe("file");

		const objPkg = resolveExtensionEntry({ package: "@pi-relay-test/fake-pack" }, tempDir);
		expect("error" in objPkg).toBe(false);
		if ("error" in objPkg) return;
		expect(objPkg.kind).toBe("package");

		const objPath = resolveExtensionEntry({ path: "/some/abs/path.ts" }, tempDir);
		expect("error" in objPath).toBe(false);
		if ("error" in objPath) return;
		expect(objPath.kind).toBe("file");
	});

	it("loads an extension from a bare package-name entry", async () => {
		const result = await loadExtensions(["@pi-relay-test/fake-pack"], tempDir);
		expect(result.errors).toEqual([]);
		expect(result.extensions).toHaveLength(1);
		const ext = result.extensions[0];
		expect(ext.path).toBe("@pi-relay-test/fake-pack");
		expect(ext.sourceInfo.source).toBe("package");
		expect(ext.toolProviders?.has("com.test.pkg-ext")).toBe(true);

		const defs = result.runtime.toolRegistry.resolve();
		expect(defs.map((d) => d.name)).toEqual(["web_search"]);
	});

	it("loads from the object form { package }", async () => {
		const result = await loadExtensions([{ package: "@pi-relay-test/fake-pack" }], tempDir);
		expect(result.errors).toEqual([]);
		expect(result.extensions).toHaveLength(1);
		expect(result.extensions[0].sourceInfo.source).toBe("package");
	});

	it("loads from the object form { path }", async () => {
		const result = await loadExtensions([{ path: path.join(pkgDir, "index.ts") }], tempDir);
		expect(result.errors).toEqual([]);
		expect(result.extensions).toHaveLength(1);
		expect(result.extensions[0].sourceInfo.source).not.toBe("package");
	});

	it("emits a diagnostic for a nonexistent package, does not crash", async () => {
		const result = await loadExtensions(["@pi-relay-test/no-such-package"], tempDir);
		expect(result.extensions).toEqual([]);
		expect(result.errors).toHaveLength(1);
		expect(result.errors[0].path).toBe("@pi-relay-test/no-such-package");
		expect(result.errors[0].error).toMatch(/Could not resolve package/);
	});
});
