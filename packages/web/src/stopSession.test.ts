import { describe, expect, it, vi } from "vitest";
import { stopSession } from "./stopSession.ts";

describe("stopSession", () => {
	it("keeps interrupt and refresh scoped to the captured session", async () => {
		let selectedSessionId = "child-a";
		const interrupt = vi.fn(async (sessionId: string) => {
			expect(sessionId).toBe("child-a");
			selectedSessionId = "parent";
		});
		const refresh = vi.fn(async () => undefined);
		const invalidateSessions = vi.fn(async () => undefined);

		await stopSession(selectedSessionId, { interrupt, refresh, invalidateSessions });

		expect(selectedSessionId).toBe("parent");
		expect(interrupt).toHaveBeenCalledWith("child-a");
		expect(refresh).toHaveBeenCalledWith("child-a");
		expect(invalidateSessions).toHaveBeenCalledOnce();
	});
});
