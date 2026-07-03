import { describe, expect, it, vi } from "vitest";
import {
	routeComposerSubmission,
	type ComposerRoutingDependencies,
	type ComposerSubmission,
} from "./composerRouting.ts";
import type { SessionSnapshot } from "./types.ts";

function snapshot(sessionId: string, parentSessionId?: string | null): SessionSnapshot {
	return {
		session_id: sessionId,
		parent_session_id: parentSessionId,
	} as SessionSnapshot;
}

function dependencies(
	loadedSnapshot: SessionSnapshot | null,
): ComposerRoutingDependencies {
	return {
		getLoadedSnapshot: () => loadedSnapshot,
		executeSlash: vi.fn(async () => undefined),
		queueFollowUp: vi.fn(async () => undefined),
		steerSubagent: vi.fn(async () => undefined),
		startNewSession: vi.fn(async () => undefined),
		reportError: vi.fn(),
	};
}

function submission(sessionId: string | null, text: string): ComposerSubmission {
	return {
		sessionId,
		text,
		clientControlId: "web_control_1",
		newSessionId: "session_1",
	};
}

describe("routeComposerSubmission", () => {
	it("steers the exact selected child through its snapshot parent", async () => {
		const deps = dependencies(snapshot("child-session", "parent-session"));

		await expect(
			routeComposerSubmission(submission("child-session", "  check the retry path  "), deps),
		).resolves.toBe(true);

		expect(deps.steerSubagent).toHaveBeenCalledWith({
			parentSessionId: "parent-session",
			subagentSessionId: "child-session",
			message: "check the retry path",
			clientControlId: "web_control_1",
		});
		expect(deps.queueFollowUp).not.toHaveBeenCalled();
		expect(deps.startNewSession).not.toHaveBeenCalled();
	});

	it("sends a root-session follow-up and never steers", async () => {
		const root = snapshot("root-session", null);
		const deps = dependencies(root);

		await expect(routeComposerSubmission(submission("root-session", "continue"), deps)).resolves.toBe(true);

		expect(deps.queueFollowUp).toHaveBeenCalledWith(
			"root-session",
			"continue",
			root,
			"web_control_1",
		);
		expect(deps.steerSubagent).not.toHaveBeenCalled();
		expect(deps.startNewSession).not.toHaveBeenCalled();
	});

	it("starts a session when none is selected", async () => {
		const deps = dependencies(null);

		await expect(
			routeComposerSubmission(submission(null, "  start here  "), deps),
		).resolves.toBe(true);

		expect(deps.startNewSession).toHaveBeenCalledWith(
			"start here",
			"web_control_1",
			"session_1",
		);
		expect(deps.queueFollowUp).not.toHaveBeenCalled();
		expect(deps.steerSubagent).not.toHaveBeenCalled();
	});

	it("retries an unchanged new-session submission with the same token and target", async () => {
		const deps = dependencies(null);
		const retry = submission(null, "start here");

		await expect(routeComposerSubmission(retry, deps)).resolves.toBe(true);
		await expect(routeComposerSubmission(retry, deps)).resolves.toBe(true);

		expect(deps.startNewSession).toHaveBeenNthCalledWith(
			1,
			"start here",
			"web_control_1",
			"session_1",
		);
		expect(deps.startNewSession).toHaveBeenNthCalledWith(
			2,
			"start here",
			"web_control_1",
			"session_1",
		);
	});

	it.each([
		["missing", null],
		["stale", snapshot("previous-child", "previous-parent")],
	])("fails safely with a %s loaded snapshot", async (_label, loadedSnapshot) => {
		const deps = dependencies(loadedSnapshot);

		await expect(
			routeComposerSubmission(submission("current-session", "do not misroute"), deps),
		).resolves.toBe(false);

		expect(deps.reportError).toHaveBeenCalledWith(expect.objectContaining({
			message: "session is still loading",
		}));
		expect(deps.queueFollowUp).not.toHaveBeenCalled();
		expect(deps.steerSubagent).not.toHaveBeenCalled();
		expect(deps.startNewSession).not.toHaveBeenCalled();
	});

	it("returns false and reports a daemon steer rejection for draft restoration", async () => {
		const rejection = new Error("cannot steer a subagent that is already terminal");
		const deps = dependencies(snapshot("child", "parent"));
		deps.steerSubagent = vi.fn(async () => {
			throw rejection;
		});

		await expect(
			routeComposerSubmission(submission("child", "one more check"), deps),
		).resolves.toBe(false);

		expect(deps.reportError).toHaveBeenCalledWith(rejection);
		expect(deps.queueFollowUp).not.toHaveBeenCalled();
	});

	it("keeps slash commands out of the steering path", async () => {
		const child = snapshot("child", "parent");
		const deps = dependencies(child);

		await expect(
			routeComposerSubmission(submission("child", "  /export  "), deps),
		).resolves.toBe(true);

		expect(deps.executeSlash).toHaveBeenCalledWith(
			{ name: "export", args: "" },
			"child",
			child,
		);
		expect(deps.steerSubagent).not.toHaveBeenCalled();
		expect(deps.queueFollowUp).not.toHaveBeenCalled();
	});

	it("keeps an accepted asynchronous steer attached to Composer's captured child", async () => {
		let appSelection = "child";
		const deps = dependencies(snapshot("child", "parent"));
		deps.steerSubagent = vi.fn(async () => {
			appSelection = "other-session";
		});

		await expect(
			routeComposerSubmission(submission(appSelection, "stay with this child"), deps),
		).resolves.toBe(true);

		expect(deps.steerSubagent).toHaveBeenCalledWith({
			parentSessionId: "parent",
			subagentSessionId: "child",
			message: "stay with this child",
			clientControlId: "web_control_1",
		});
		expect(appSelection).toBe("other-session");
	});

	it("never retargets when App has already moved to another child", async () => {
		const snapshots = new Map([
			["old-child", snapshot("old-child", "old-parent")],
			["new-child", snapshot("new-child", "new-parent")],
		]);
		let appSelection = "new-child";
		const deps = dependencies(null);
		deps.getLoadedSnapshot = (capturedSessionId) => {
			expect(capturedSessionId).toBe("old-child");
			return snapshots.get(capturedSessionId) ?? null;
		};

		await expect(
			routeComposerSubmission(submission("old-child", "stay old"), deps),
		).resolves.toBe(true);

		expect(appSelection).toBe("new-child");
		expect(deps.steerSubagent).toHaveBeenCalledWith({
			parentSessionId: "old-parent",
			subagentSessionId: "old-child",
			message: "stay old",
			clientControlId: "web_control_1",
		});
	});

	it("fails under the captured draft when its snapshot was evicted after selection changed", async () => {
		const deps = dependencies(null);

		await expect(
			routeComposerSubmission(submission("old-child", "restore me"), deps),
		).resolves.toBe(false);

		expect(deps.reportError).toHaveBeenCalledWith(
			expect.objectContaining({ message: "session is still loading" }),
		);
		expect(deps.steerSubagent).not.toHaveBeenCalled();
		expect(deps.queueFollowUp).not.toHaveBeenCalled();
	});
});
