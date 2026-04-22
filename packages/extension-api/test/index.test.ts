import { describe, expect, expectTypeOf, it } from "vitest";
import type { ExtensionAPI, ExtensionFactory } from "../src/index.js";

describe("@pi-relay/extension-api", () => {
	it("keeps ExtensionFactory aligned with ExtensionAPI", () => {
		expectTypeOf<ExtensionFactory>().toEqualTypeOf<(pi: ExtensionAPI) => void | Promise<void>>();
		expect(true).toBe(true);
	});
});
