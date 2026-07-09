import { describe, expect, it } from "vitest";
import {
	browserWorkspaceRouteHistory,
	fallbackExecutionConversation,
	hostRouteScope,
	legacyWorkspaceResume,
	messageRecipient,
	openAgentConversation,
	parseWorkspaceRoute,
	projectRouteScope,
	rootConversationRoute,
	selectRootRun,
	serializeWorkspaceRoute,
	unavailableConversationRoute,
	unavailableExecutionDetail,
	type ConversationRoute,
	type ExecutionRoute,
	type ExecutionView,
	type WorkspaceRoute,
	type WorkspaceRouteMatch,
	type WorkspaceRouteScope,
} from "./workspaceRoute.ts";

const PROJECT = projectRouteScope("project-1");
const HOST = hostRouteScope();

describe("static/SSR safety", () => {
	it("imports without browser globals and returns no browser adapter in Node", () => {
		expect(typeof window).toBe("undefined");
		expect(browserWorkspaceRouteHistory()).toBeNull();
	});
});

describe("workspace route parser and serializer", () => {
	it("distinguishes paths outside the owned workspace namespace", () => {
		expect(parseWorkspaceRoute("/")).toEqual({ kind: "none" });
		expect(parseWorkspaceRoute("/settings?conversation=%ZZ")).toEqual({ kind: "none" });
		expect(parseWorkspaceRoute("/work/project-1")).toEqual({ kind: "none" });
	});

	it.each([
		[PROJECT, "/w/project/project-1/run/root-1/conversation/root-1"],
		[HOST, "/w/host/run/root-1/conversation/root-1"],
	] as const)("round-trips root Conversation in project and Host scope", (scope, expectedUrl) => {
		const route = rootConversationRoute(scope, "root-1");

		expect(serializeWorkspaceRoute(route)).toBe(expectedUrl);
		expect(expectRoute(expectedUrl)).toEqual(route);
	});

	it("safely encodes and decodes valid reserved characters in every path identifier", () => {
		const root = rootConversationRoute(projectRouteScope("project ?#% ü"), "root ?#% ü");
		const route = expectConversation(openAgentConversation(root, "agent ?#% ü").route);
		const url = serializeWorkspaceRoute(route);

		expect(url).toBe(
			"/w/project/project%20%3F%23%25%20%C3%BC/run/root%20%3F%23%25%20%C3%BC/conversation/agent%20%3F%23%25%20%C3%BC",
		);
		expect(expectRoute(url)).toEqual(route);
	});

	it("round-trips every scope/subview/conversation/focus/handoff combination", () => {
		const scopes = [PROJECT, HOST];
		const conversations: ExecutionRoute["conversation"][] = [
			{ kind: "root" },
			expectExecution(
				expectRoute(
					"/w/host/run/root-1/execution/overview?conversation=agent%3Aagent-1",
				),
			).conversation,
		];
		const focuses: ExecutionRoute["focus"][] = [
			{ kind: "root" },
			expectExecution(expectRoute("/w/host/run/root-1/execution/overview?focus=delegation%3Adelegation-1")).focus,
			expectExecution(expectRoute("/w/host/run/root-1/execution/overview?focus=agent%3Aagent-1")).focus,
		];
		const handoffs: ExecutionRoute["handoff"][] = [
			null,
			expectExecution(expectRoute("/w/host/run/root-1/execution/overview?handoff=handoff-1")).handoff,
		];

		for (const scope of scopes) {
			for (const view of ["overview", "activity", "handoffs"] as const) {
				for (const conversation of conversations) {
					for (const focus of focuses) {
						for (const handoff of handoffs) {
							const route: ExecutionRoute = {
								...executionRoute(scope, view),
								conversation,
								focus,
								handoff,
							};
							expect(expectRoute(serializeWorkspaceRoute(route))).toEqual(route);
						}
					}
				}
			}
		}
	});

	it("round-trips typed conversation, focus, and handoff in deterministic parameter order", () => {
		const route = expectExecution(expectRoute(
			"/w/project/project-1/run/root-1/execution/handoffs" +
				"?conversation=agent%3Aagent%20%3F1" +
				"&focus=agent%3Aagent%20%3F1" +
				"&handoff=delegation%201%20final_message.md",
		));
		const url = serializeWorkspaceRoute(route);

		expect(url).toBe(
			"/w/project/project-1/run/root-1/execution/handoffs" +
				"?conversation=agent%3Aagent%20%3F1" +
				"&focus=agent%3Aagent%20%3F1" +
				"&handoff=delegation%201%20final_message.md",
		);
		expect(expectRoute(url)).toEqual(route);
	});

	it("canonicalizes explicit root conversation and focus defaults to omitted parameters", () => {
		const result = expectMatch(
			"/w/host/run/root-1/execution/overview?focus=root%3Aroot-1&conversation=root%3Aroot-1",
		);

		expect(result.route).toEqual(executionRoute(HOST, "overview"));
		expect(result.canonicalUrl).toBe("/w/host/run/root-1/execution/overview");
		expect(result.correction).toEqual({
			kind: "replace",
			url: "/w/host/run/root-1/execution/overview",
			reasons: ["explicit-conversation-default", "explicit-focus-default"],
		});
		expect(expectRoute(result.canonicalUrl)).toEqual(result.route);
	});

	it("canonicalizes explicit Outline but does not enable Map", () => {
		const outline = expectMatch("/w/host/run/root-1/execution/overview?overview=outline");
		expect(outline.correction).toMatchObject({
			kind: "replace",
			reasons: ["explicit-overview-default"],
		});

		const map = parseWorkspaceRoute("/w/host/run/root-1/execution/overview?overview=map");
		expect(map).toMatchObject({
			kind: "unavailable",
			issue: "unsupported-overview",
			backTo: {
				label: "root-outline",
				url: "/w/host/run/root-1/execution/overview",
			},
		});
	});

	it.each([
		["conversation=untyped", "malformed"],
		["conversation=root%3Aother-root", "wrong-root"],
		["conversation=agent%3A", "malformed"],
		["conversation=agent%3Aroot-1", "malformed"],
		["conversation=agent%3Achild-1&conversation=agent%3Achild-2", "malformed"],
	] as const)(
		"falls back visibly and canonically for invalid optional Execution conversation %s",
		(query, reason) => {
			const result = expectMatch(
				`/w/project/project-1/run/root-1/execution/activity?${query}&focus=delegation%3Awork-1`,
			);

			expect(result.route).toMatchObject({
				destination: "execution",
				conversation: { kind: "root" },
				focus: { kind: "delegation", delegationId: "work-1" },
			});
			expect(result.warnings[0]).toMatchObject({
				kind: "invalid-execution-conversation",
				persistent: true,
				reason,
			});
			expect(result.correction).toEqual({
				kind: "replace",
				url: "/w/project/project-1/run/root-1/execution/activity?focus=delegation%3Awork-1",
				reasons: ["invalid-conversation-fallback"],
			});
		},
	);

	it("converts integration-discovered unknown/wrong-root membership to the same warning and replacement", () => {
		const route = expectExecution(
			expectRoute(
				"/w/host/run/root-1/execution/activity?conversation=agent%3Achild-1&focus=agent%3Achild-1",
			),
		);
		const fallback = fallbackExecutionConversation(route, "wrong-root-membership");

		expect(fallback.route).toMatchObject({
			conversation: { kind: "root" },
			focus: { kind: "agent", sessionId: "child-1" },
		});
		expect(fallback.warnings).toEqual([
			expect.objectContaining({
				kind: "invalid-execution-conversation",
				reason: "wrong-root-membership",
				requestedValue: "agent:child-1",
				persistent: true,
			}),
		]);
		expect(fallback.correction?.url).toBe(
			"/w/host/run/root-1/execution/activity?focus=agent%3Achild-1",
		);
	});

	it.each([
		"/w/host/run/root-1/conversation",
		"/w/host/run/root-1/conversation/",
		"/w/host/run/root-1/conversation/child-1/extra",
		"/w/host/run/root-1/conversation/%00",
		"/w/host/run/root-1/conversation/%20%20",
	] as const)("keeps an invalid required Conversation path unavailable: %s", (url) => {
		const result = parseWorkspaceRoute(url);

		expect(result).toMatchObject({
			kind: "unavailable",
			issue: "invalid-conversation",
			backTo: {
				label: "root-conversation",
				url: "/w/host/run/root-1/conversation/root-1",
			},
		});
		expect(result).not.toHaveProperty("correction");
	});

	it("converts integration-discovered invalid Conversation membership to unavailable without replacement", () => {
		const route = expectConversation(expectRoute("/w/host/run/root-1/conversation/child-1"));
		const result = unavailableConversationRoute(route);

		expect(result).toEqual({
			kind: "unavailable",
			issue: "invalid-conversation",
			message: "The requested conversation is unavailable for this root run.",
			requestedUrl: "/w/host/run/root-1/conversation/child-1",
			backTo: {
				label: "root-conversation",
				url: "/w/host/run/root-1/conversation/root-1",
			},
		});
	});

	it.each([
		"/w/project//run/root-1/conversation/root-1",
		"/w/project/project-1/run//conversation/root-1",
		"/w/host/run//execution/overview",
		"/w/host/run/root-1/execution",
		"/w/host/run/root-1/execution/overview/extra",
		"/w/host/root-1/execution/overview",
	] as const)("rejects malformed or extra required path segments as owned unavailable: %s", (url) => {
		expect(parseWorkspaceRoute(url)).toMatchObject({ kind: "unavailable", issue: "invalid-path" });
	});

	it.each([
		"/w/host/run/%ZZ/conversation/root-1",
		"/w/project/project-1/run/root-1/conversation/%E0%A4%A",
	] as const)("rejects invalid percent encoding in an owned path: %s", (url) => {
		const result = parseWorkspaceRoute(url);
		expect(result).toMatchObject({
			kind: "unavailable",
			issue: "invalid-path-encoding",
		});
		if (url.includes("/conversation/%E0")) {
			expect(result).toMatchObject({
				backTo: {
					label: "root-conversation",
					url: "/w/project/project-1/run/root-1/conversation/root-1",
				},
			});
		}
	});

	it.each([
		"focus=plain",
		"focus=root%3Aother-root",
		"focus=delegation%3A",
		"focus=agent%3Aroot-1",
		"focus=agent%3Achild-1&focus=agent%3Achild-2",
	] as const)("renders malformed focus as an owned unavailable detail: %s", (query) => {
		const result = parseWorkspaceRoute(`/w/host/run/root-1/execution/activity?${query}`);

		expect(result).toMatchObject({
			kind: "unavailable",
			issue: "invalid-focus",
			backTo: { label: "root-outline", url: "/w/host/run/root-1/execution/overview" },
		});
	});

	it("parses an opaque valid handoff ref and rejects malformed/duplicate values", () => {
		const valid = expectExecution(
			expectRoute("/w/host/run/root-1/execution/handoffs?handoff=cancelled-agent-1.transcript.md"),
		);
		expect(valid.handoff).toEqual({
			kind: "handoff",
			ref: "cancelled-agent-1.transcript.md",
		});

		for (const query of [
			"handoff=",
			"handoff=.",
			"handoff=..",
			"handoff=%2F",
			"handoff=%00",
			"handoff=%20%20",
			"handoff=one&handoff=two",
		]) {
			expect(parseWorkspaceRoute(`/w/host/run/root-1/execution/handoffs?${query}`)).toMatchObject({
				kind: "unavailable",
				issue: "invalid-handoff",
				backTo: { label: "root-outline" },
			});
		}
	});

	it("preserves unavailable focus/handoff selection for owned detail rendering", () => {
		const route = expectExecution(expectRoute(
			"/w/project/project-1/run/root-1/execution/handoffs" +
				"?conversation=agent%3Achild-1&focus=delegation%3Adelegation-1&handoff=missing-file",
		));

		expect(unavailableExecutionDetail(route, "focus")).toMatchObject({
			kind: "unavailable",
			issue: "invalid-focus",
			requestedUrl:
				"/w/project/project-1/run/root-1/execution/handoffs" +
				"?conversation=agent%3Achild-1&focus=delegation%3Adelegation-1&handoff=missing-file",
			backTo: {
				label: "root-outline",
				url: "/w/project/project-1/run/root-1/execution/overview",
			},
		});
		expect(unavailableExecutionDetail(route, "handoff")).toMatchObject({
			kind: "unavailable",
			issue: "invalid-handoff",
		});
	});

	it("rejects malformed query encoding as an owned unavailable state", () => {
		expect(
			parseWorkspaceRoute("/w/host/run/root-1/execution/overview?focus=agent%3A%E0%A4%A"),
		).toMatchObject({
			kind: "unavailable",
			issue: "invalid-query-encoding",
			backTo: { label: "root-outline" },
		});
		expect(
			parseWorkspaceRoute("/w/host/run/root-1/conversation/root-1?extra=%ZZ"),
		).toMatchObject({
			kind: "unavailable",
			issue: "invalid-query-encoding",
			backTo: { label: "root-conversation" },
		});
	});

	it("strips and explicitly flags unsupported query/fragment state instead of carrying it", () => {
		const result = expectMatch(
			"/w/host/run/root-1/execution/activity" +
				"?z=last&focus=delegation%3Awork-1&recipient=child-1&a=first#debug",
		);

		expect(result.canonicalUrl).toBe(
			"/w/host/run/root-1/execution/activity?focus=delegation%3Awork-1",
		);
		expect(result.warnings).toEqual([
			{
				kind: "unsupported-query",
				persistent: false,
				parameters: ["a", "recipient", "z"],
				message: "Unsupported workspace query parameters were removed.",
			},
			{
				kind: "unsupported-fragment",
				persistent: false,
				message: "URL fragments are not supported for workspace routes and were removed.",
			},
		]);
		expect(result.correction?.reasons).toEqual(["unsupported-query", "unsupported-fragment"]);
	});

	it("strips every query parameter from Conversation routes", () => {
		const result = expectMatch(
			"/w/host/run/root-1/conversation/root-1?conversation=agent%3Achild&focus=agent%3Achild",
		);

		expect(result.canonicalUrl).toBe("/w/host/run/root-1/conversation/root-1");
		expect(result.warnings).toEqual([
			expect.objectContaining({
				kind: "unsupported-query",
				parameters: ["conversation", "focus"],
			}),
		]);
	});

	it("canonicalizes alternate safe encodings and parameter order deterministically", () => {
		const result = expectMatch(
			"/w/host/run/%72oot-1/execution/activity" +
				"?handoff=file&focus=agent%3achild&conversation=agent%3achild",
		);

		expect(result.canonicalUrl).toBe(
			"/w/host/run/root-1/execution/activity" +
				"?conversation=agent%3Achild&focus=agent%3Achild&handoff=file",
		);
		expect(result.correction?.reasons).toEqual(["noncanonical-url"]);
		expect(expectRoute(result.canonicalUrl)).toEqual(result.route);
	});

	it("rejects contradictory or obsolete forged route state instead of silently discarding it", () => {
		const valid = executionRoute(HOST, "overview");
		const forged: unknown[] = [
			{ ...valid, conversation: { kind: "root", sessionId: "other-root" } },
			{ ...valid, focus: { kind: "root", sessionId: "other-root" } },
			{ ...valid, conversation: { kind: "agent", sessionId: "root-1" } },
			{ ...valid, focus: { kind: "agent", sessionId: "root-1" } },
			{ ...valid, conversation: { kind: "agent", sessionId: "child-1", membership: "validated" } },
			{ ...valid, focus: { kind: "agent", sessionId: "child-1", availability: "validated" } },
			{ ...valid, handoff: { kind: "handoff", ref: "detail-1", availability: "validated" } },
			{
				...rootConversationRoute(HOST, "root-1"),
				conversationSessionId: "other-session",
			},
		];

		for (const route of forged) {
			expect(() => serializeWorkspaceRoute(route as WorkspaceRoute)).toThrowError(
				/Workspace route programmer error/,
			);
		}
	});

	it("rejects malformed IDs in every serializer identity position", () => {
		const valid = executionRoute(PROJECT, "overview");
		const forged: unknown[] = [
			{ ...valid, rootSessionId: ".." },
			{ ...valid, scope: { kind: "project", projectId: "bad/project" } },
			{ ...valid, conversation: { kind: "agent", sessionId: "bad\u0000conversation" } },
			{ ...valid, focus: { kind: "agent", sessionId: "bad/focus" } },
			{ ...valid, focus: { kind: "delegation", delegationId: "  " } },
			{ ...valid, handoff: { kind: "handoff", ref: "." } },
		];

		for (const route of forged) {
			expect(() => serializeWorkspaceRoute(route as WorkspaceRoute)).toThrowError(
				/Workspace route programmer error/,
			);
		}
	});

	it("makes contradictory root and membership shapes type errors", () => {
		// @ts-expect-error Root conversation identity is derived from rootSessionId.
		const duplicateRootConversation: ExecutionRoute["conversation"] = { kind: "root", sessionId: "other" };
		// @ts-expect-error Root focus identity is derived from rootSessionId.
		const duplicateRootFocus: ExecutionRoute["focus"] = { kind: "root", sessionId: "other" };
		const agentConversation = agentExecutionRoute(HOST, "overview", "child").conversation;
		const writableMembership: ExecutionRoute["conversation"] = {
			...agentConversation,
			// @ts-expect-error Membership validation is not freely writable route identity.
			membership: "validated",
		};
		const childFocus = expectExecution(
			expectRoute("/w/host/run/root-1/execution/overview?focus=agent%3Achild"),
		).focus;
		if (childFocus.kind !== "agent") throw new Error("expected agent focus");
		const writableAvailability: ExecutionRoute["focus"] = {
			kind: "agent",
			sessionId: childFocus.sessionId,
			// @ts-expect-error Availability validation is not freely writable route identity.
			availability: "validated",
		};
		expect([
			duplicateRootConversation,
			duplicateRootFocus,
			writableMembership,
			writableAvailability,
		]).toHaveLength(4);
	});

	it.each([
		"",
		"  ",
		".",
		"..",
		"bad/id",
		"bad\\id",
		"bad\u0000id",
		"bad\u007fid",
		"bad\u0085id",
		"bad\ud800id",
	])(
		"active outgoing builders reject malformed IDs consistently: %j",
		(id) => {
			expect(() => projectRouteScope(id)).toThrowError(/Workspace route programmer error/);
			expect(() => rootConversationRoute(HOST, id)).toThrowError(/Workspace route programmer error/);
			expect(() => selectRootRun(HOST, id)).toThrowError(/Workspace route programmer error/);

			const root = rootConversationRoute(HOST, "root-1");
			expect(() => openAgentConversation(root, id)).toThrowError(/Workspace route programmer error/);
		},
	);

	it("rejects malformed IDs from URLs with the same rules used by builders", () => {
		expect(parseWorkspaceRoute("/w/project/%2E/run/root-1/conversation/root-1")).toMatchObject({
			kind: "unavailable",
			issue: "invalid-path",
		});
		expect(parseWorkspaceRoute("/w/host/run/%2E%2E/conversation/root-1")).toMatchObject({
			kind: "unavailable",
			issue: "invalid-path",
		});
		expect(parseWorkspaceRoute("/w/host/run/root-1/conversation/child%2Fone")).toMatchObject({
			kind: "unavailable",
			issue: "invalid-conversation",
		});
		expect(
			parseWorkspaceRoute("/w/host/run/root-1/execution/overview?conversation=agent%3Achild%2Fone"),
		).toMatchObject({
			kind: "route",
			warnings: [expect.objectContaining({ reason: "malformed" })],
		});
		for (const query of [
			"focus=agent%3Achild%2Fone",
			"focus=delegation%3Awork%2Fone",
			"handoff=final%2Fmessage",
		]) {
			expect(parseWorkspaceRoute(`/w/host/run/root-1/execution/overview?${query}`)).toMatchObject({
				kind: "unavailable",
			});
		}
	});

	it.each([".", "..", "%2e", "%2e%2e"])(
		"never emits browser-normalized dot-segment ID %s",
		(segment) => {
			const decoded = decodeURIComponent(segment);
			expect(() => openAgentConversation(rootConversationRoute(HOST, "root-1"), decoded)).toThrowError(
				/Workspace route programmer error/,
			);
			const browserPath = new URL(
				`/w/host/run/root-1/conversation/${segment}`,
				"https://example.test",
			).pathname;
			expect(browserPath).not.toBe(`/w/host/run/root-1/conversation/${segment}`);
		},
	);
});

describe("workspace route transitions and recipient derivation", () => {
	it("derives recipient exclusively from the effective conversation, never focus", () => {
		const rootExecution = expectExecution(expectRoute(
			"/w/project/project-1/run/root-1/execution/overview?focus=agent%3Afocused-child",
		));
		expect(messageRecipient(rootExecution)).toEqual({ kind: "root", sessionId: "root-1" });

		const agentExecution: ExecutionRoute = {
			...rootExecution,
			conversation: agentExecutionRoute(PROJECT, "overview", "recipient-child").conversation,
		};
		expect(messageRecipient(agentExecution)).toEqual({
			kind: "agent",
			sessionId: "recipient-child",
		});
		expect(messageRecipient(rootConversationRoute(HOST, "root-1"))).toEqual({
			kind: "root",
			sessionId: "root-1",
		});
	});
});

describe("legacy route resume migration", () => {
	it("always lets a valid or unavailable URL route win over legacy selection", () => {
		const valid = expectMatch("/w/host/run/url-root/conversation/url-root");
		expect(
			legacyWorkspaceResume(valid, { projectId: "legacy-project", sessionId: "legacy-session" }, {
				kind: "known",
				rootSessionId: "legacy-root",
			}),
		).toEqual({ kind: "url", result: valid });

		const unavailable = parseWorkspaceRoute("/w/host/run/url-root/conversation");
		expect(
			legacyWorkspaceResume(unavailable, { projectId: null, sessionId: "legacy-session" }),
		).toEqual({ kind: "url", result: unavailable });
	});

	it("does not guess that a legacy selected subagent is a root", () => {
		expect(
			legacyWorkspaceResume(
				{ kind: "none" },
				{ projectId: "project-1", sessionId: "selected-child" },
			),
		).toEqual({
			kind: "needs-root-resolution",
			scope: { kind: "project", projectId: "project-1" },
			selectedSessionId: "selected-child",
		});
	});

	it.each([
		[
			{ projectId: null, sessionId: "selected-child" },
			"resolved-root",
			"/w/host/run/resolved-root/conversation/selected-child",
			{ kind: "agent", sessionId: "selected-child" },
		],
		[
			{ projectId: "project-1", sessionId: "selected-child" },
			"resolved-root",
			"/w/project/project-1/run/resolved-root/conversation/selected-child",
			{ kind: "agent", sessionId: "selected-child" },
		],
		[
			{ projectId: null, sessionId: "resolved-root" },
			"resolved-root",
			"/w/host/run/resolved-root/conversation/resolved-root",
			{ kind: "root" },
		],
	] as const)(
		"pins the resolved root while preserving the legacy selected conversation",
		(selection, rootSessionId, expectedUrl, conversation) => {
			const result = legacyWorkspaceResume(
				{ kind: "none" },
				selection,
				{ kind: "known", rootSessionId },
			);
			expect(result).toEqual({
				kind: "legacy-route",
				navigation: expect.objectContaining({
					kind: "route",
					history: "replace",
					url: expectedUrl,
					route: expect.objectContaining({
						rootSessionId,
						conversation,
					}),
				}),
			});
			if (result.kind !== "legacy-route") throw new Error("expected a migrated legacy route");
			expect(messageRecipient(result.navigation.route)).toEqual({
				kind: conversation.kind,
				sessionId: selection.sessionId,
			});
		},
	);

	it.each([
		{ projectId: "project-1", sessionId: "selected-child" },
		{ projectId: null, sessionId: "selected-child" },
	] as const)("keeps a resolver failure explicit instead of guessing a root", (selection) => {
		expect(
			legacyWorkspaceResume(
				{ kind: "none" },
				selection,
				{ kind: "failed" },
			),
		).toEqual({
			kind: "root-resolution-failed",
			scope:
				selection.projectId === null
					? { kind: "host" }
					: { kind: "project", projectId: "project-1" },
			selectedSessionId: "selected-child",
		});
	});

	it("has no fallback when legacy storage has no selected session", () => {
		expect(
			legacyWorkspaceResume(
				{ kind: "none" },
				{ projectId: "project-1", sessionId: null },
			),
		).toEqual({ kind: "empty" });
	});
});

function executionRoute(scope: WorkspaceRouteScope, view: ExecutionView): ExecutionRoute {
	const prefix = scope.kind === "project"
		? `/w/project/${scope.projectId}/run/root-1`
		: "/w/host/run/root-1";
	return expectExecution(expectRoute(`${prefix}/execution/${view}`));
}

function agentExecutionRoute(
	scope: WorkspaceRouteScope,
	view: ExecutionView,
	sessionId: string,
): ExecutionRoute {
	const prefix = scope.kind === "project"
		? `/w/project/${scope.projectId}/run/root-1`
		: "/w/host/run/root-1";
	return expectExecution(expectRoute(
		`${prefix}/execution/${view}?conversation=agent%3A${encodeURIComponent(sessionId)}`,
	));
}

function expectMatch(url: string): WorkspaceRouteMatch {
	const result = parseWorkspaceRoute(url);
	expect(result.kind).toBe("route");
	return result as WorkspaceRouteMatch;
}

function expectRoute(url: string): WorkspaceRoute {
	return expectMatch(url).route;
}

function expectConversation(route: WorkspaceRoute): ConversationRoute {
	expect(route.destination).toBe("conversation");
	return route as ConversationRoute;
}

function expectExecution(route: WorkspaceRoute): ExecutionRoute {
	expect(route.destination).toBe("execution");
	return route as ExecutionRoute;
}
