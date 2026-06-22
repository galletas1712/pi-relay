import { describe, expect, it } from "vitest";
import { sessionStatusWithDelegations } from "./sessionList.ts";

describe("sessionStatusWithDelegations", () => {
	it("is idle when the parent is idle and no delegations run", () => {
		expect(sessionStatusWithDelegations("idle", false)).toBe("idle");
	});

	it("is delegating when the parent is idle but subagents are in flight", () => {
		expect(sessionStatusWithDelegations("idle", true)).toBe("delegating");
	});

	it("is running when the parent itself is running, regardless of delegations", () => {
		expect(sessionStatusWithDelegations("running", false)).toBe("running");
		expect(sessionStatusWithDelegations("running", true)).toBe("running");
	});

	it("treats a queued parent as running even with delegations in flight", () => {
		expect(sessionStatusWithDelegations("queued", true)).toBe("running");
	});
});
