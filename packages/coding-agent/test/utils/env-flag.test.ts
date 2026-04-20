import { describe, expect, it } from "vitest";
import { isTruthyEnvFlag } from "../../src/utils/env-flag.js";

describe("isTruthyEnvFlag", () => {
	it.each<[string | undefined, boolean]>([
		["1", true],
		["true", true],
		["TRUE", true], // case-insensitive
		["True", true],
		["yes", true],
		["YES", true],
		["Yes", true],
		["0", false],
		["false", false],
		["FALSE", false],
		["no", false],
		["NO", false],
		["", false],
		[undefined, false],
		["maybe", false],
		["2", false],
		["on", false], // deliberately not in the accepted set
		["off", false],
	])("isTruthyEnvFlag(%j) → %j", (input, expected) => {
		expect(isTruthyEnvFlag(input)).toBe(expected);
	});
});
