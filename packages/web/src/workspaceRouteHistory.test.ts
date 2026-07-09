import { describe, expect, it, vi } from "vitest";
import {
	agentFocus,
	changeExecutionFocus,
	changeExecutionView,
	closeHandoff,
	handoffReference,
	hostRouteScope,
	messageAgent,
	openAgentConversation,
	openHandoff,
	projectRouteScope,
	rootConversationRoute,
	selectRootRun,
	showConversation,
	showExecution,
	WorkspaceRouteHistory,
	type ExecutionRoute,
	type WorkspaceHistoryLike,
	type WorkspacePopstateSource,
	type WorkspaceRouteHistoryDependencies,
	type WorkspaceRouteLocation,
	type WorkspaceRouteParseResult,
} from "./workspaceRoute.ts";

describe("WorkspaceRouteHistory", () => {
	it("pushes destinations/conversations/details, replaces focus, and preserves browser length semantics", () => {
		const browser = new FakeBrowser("/");
		const adapter = new WorkspaceRouteHistory(browser.dependencies);

		adapter.apply(selectRootRun(projectRouteScope("project-1"), "root-1"));
		expect(browser.currentUrl).toBe("/w/project/project-1/run/root-1/conversation/root-1");
		expect(browser.length).toBe(2);

		const conversation = expectRoute(adapter.current()).route;
		const executionNavigation = showExecution(conversation, "overview");
		adapter.apply(executionNavigation);
		expect(browser.length).toBe(3);

		const execution = expectExecution(expectRoute(adapter.current()).route);
		adapter.apply(changeExecutionFocus(execution, agentFocus("child-1")));
		expect(browser.currentUrl).toContain("?focus=agent%3Achild-1");
		expect(browser.length).toBe(3);

		const focused = expectExecution(expectRoute(adapter.current()).route);
		adapter.apply(openHandoff(focused, handoffReference("final-message")));
		expect(browser.currentUrl).toContain("&handoff=final-message");
		expect(browser.length).toBe(4);

		const detail = expectExecution(expectRoute(adapter.current()).route);
		expect(adapter.apply(closeHandoff(detail))).toBeNull();
		expect(browser.currentUrl).toBe(
			"/w/project/project-1/run/root-1/execution/overview?focus=agent%3Achild-1",
		);
		expect(browser.length).toBe(4);
		expect(browser.index).toBe(2);

		expect(browser.pushCalls).toHaveLength(3);
		expect(browser.replaceCalls).toHaveLength(1);
		expect(browser.backCalls).toBe(1);
	});

	it("uses push for every explicit destination/conversation/subview/durable-detail action", () => {
		const browser = new FakeBrowser("/");
		const adapter = new WorkspaceRouteHistory(browser.dependencies);
		const root = rootConversationRoute(hostRouteScope(), "root-1");

		adapter.apply(selectRootRun(hostRouteScope(), "root-1"));
		const execution = expectExecution(showExecution(root, "overview").route);
		adapter.apply(showExecution(root, "overview"));
		adapter.apply(changeExecutionView(execution, "activity"));
		adapter.apply(showConversation(execution));
		adapter.apply(openAgentConversation(execution, "child-1"));
		adapter.apply(messageAgent(execution, "child-2"));
		adapter.apply(openHandoff({ ...execution, view: "handoffs" }, handoffReference("detail-1")));

		expect(browser.pushCalls).toHaveLength(7);
		expect(browser.replaceCalls).toHaveLength(0);
		expect(browser.length).toBe(8);
	});

	it("applies canonical/default/invalid-optional corrections with replace and no extra Back entry", () => {
		const requested =
			"/w/host/run/root-1/execution/activity" +
			"?conversation=root%3Aother-root&focus=root%3Aroot-1&unsupported=yes";
		const browser = new FakeBrowser(requested);
		const adapter = new WorkspaceRouteHistory(browser.dependencies);
		const parsed = expectRoute(adapter.current());

		expect(parsed.warnings).toEqual([
			expect.objectContaining({
				kind: "invalid-execution-conversation",
				persistent: true,
				reason: "wrong-root",
			}),
			expect.objectContaining({
				kind: "unsupported-query",
				parameters: ["unsupported"],
			}),
		]);
		const corrected = expectRoute(adapter.correct(parsed));

		expect(browser.currentUrl).toBe("/w/host/run/root-1/execution/activity");
		expect(browser.length).toBe(1);
		expect(browser.replaceCalls).toHaveLength(1);
		expect(browser.pushCalls).toHaveLength(0);
		expect(corrected.correction).toBeNull();
		expect(corrected.warnings).toEqual(parsed.warnings);
	});

	it("restores one atomic parsed state per Back/Forward without issuing route mutations", () => {
		const browser = new FakeBrowser("/");
		const adapter = new WorkspaceRouteHistory(browser.dependencies);
		const observed: WorkspaceRouteParseResult[] = [];
		const listener = vi.fn((result: WorkspaceRouteParseResult) => observed.push(result));
		const unsubscribe = adapter.subscribe(listener);

		adapter.apply(selectRootRun(hostRouteScope(), "root-1"));
		const rootConversation = expectRoute(adapter.current()).route;
		adapter.apply(showExecution(rootConversation, "activity"));
		const execution = expectExecution(expectRoute(adapter.current()).route);
		adapter.apply(changeExecutionFocus(execution, agentFocus("child-1")));
		const focused = expectExecution(expectRoute(adapter.current()).route);
		adapter.apply(openHandoff(focused, handoffReference("detail-1")));

		const pushCount = browser.pushCalls.length;
		const replaceCount = browser.replaceCalls.length;
		browser.back();

		expect(listener).toHaveBeenCalledTimes(1);
		expect(expectRoute(observed[0]).route).toMatchObject({
			destination: "execution",
			view: "activity",
			conversation: { kind: "root" },
			focus: { kind: "agent", sessionId: "child-1" },
			handoff: null,
		});
		expect(browser.pushCalls).toHaveLength(pushCount);
		expect(browser.replaceCalls).toHaveLength(replaceCount);

		browser.back();
		expect(listener).toHaveBeenCalledTimes(2);
		expect(expectRoute(observed[1]).route).toMatchObject({
			destination: "conversation",
			rootSessionId: "root-1",
			conversation: { kind: "root" },
		});

		browser.forward();
		expect(listener).toHaveBeenCalledTimes(3);
		expect(expectRoute(observed[2]).route).toMatchObject({
			destination: "execution",
			focus: { kind: "agent", sessionId: "child-1" },
		});
		expect(browser.pushCalls).toHaveLength(pushCount);
		expect(browser.replaceCalls).toHaveLength(replaceCount);

		unsubscribe();
		browser.forward();
		expect(listener).toHaveBeenCalledTimes(3);
	});

	it("replaces a directly loaded handoff on Close instead of leaving the workspace", () => {
		const direct =
			"/w/host/run/root-1/execution/handoffs" +
			"?conversation=agent%3Achild-1&focus=agent%3Achild-1&handoff=final-message";
		const browser = new FakeBrowser(direct);
		const adapter = new WorkspaceRouteHistory(browser.dependencies);
		const route = expectExecution(expectRoute(adapter.current()).route);
		const result = adapter.apply(closeHandoff(route));

		expect(result).toMatchObject({ kind: "route", route: { handoff: null } });
		expect(browser.currentUrl).toBe(
			"/w/host/run/root-1/execution/handoffs" +
				"?conversation=agent%3Achild-1&focus=agent%3Achild-1",
		);
		expect(browser.length).toBe(1);
		expect(browser.backCalls).toBe(0);
		expect(browser.replaceCalls).toHaveLength(1);
	});

	it("closes to the new focus in place after focus replacement while a handoff is open", () => {
		const browser = new FakeBrowser("/w/host/run/root-1/execution/handoffs?focus=agent%3Achild-1");
		const adapter = new WorkspaceRouteHistory(browser.dependencies);
		const initial = expectExecution(expectRoute(adapter.current()).route);

		adapter.apply(openHandoff(initial, handoffReference("final-message")));
		expect(browser.length).toBe(2);
		const detail = expectExecution(expectRoute(adapter.current()).route);
		adapter.apply(changeExecutionFocus(detail, agentFocus("child-2")));
		expect(browser.currentUrl).toBe(
			"/w/host/run/root-1/execution/handoffs?focus=agent%3Achild-2&handoff=final-message",
		);
		expect(browser.length).toBe(2);

		const refocusedDetail = expectExecution(expectRoute(adapter.current()).route);
		const result = adapter.apply(closeHandoff(refocusedDetail));

		expect(result).toMatchObject({
			kind: "route",
			route: { focus: { kind: "agent", sessionId: "child-2" }, handoff: null },
		});
		expect(browser.currentUrl).toBe(
			"/w/host/run/root-1/execution/handoffs?focus=agent%3Achild-2",
		);
		expect(browser.length).toBe(2);
		expect(browser.index).toBe(1);
		expect(browser.backCalls).toBe(0);
	});

	it("parses location lazily and does not cache a stale route", () => {
		const browser = new FakeBrowser("/w/host/run/root-1/conversation/root-1");
		const adapter = new WorkspaceRouteHistory(browser.dependencies);

		expect(expectRoute(adapter.current()).route).toMatchObject({ rootSessionId: "root-1" });
		browser.externalReplace("/w/host/run/root-2/conversation/root-2");
		expect(expectRoute(adapter.current()).route).toMatchObject({ rootSessionId: "root-2" });
	});
});

class FakeBrowser implements WorkspaceHistoryLike, WorkspacePopstateSource {
	readonly location: WorkspaceRouteLocation = { pathname: "/", search: "", hash: "" };
	readonly pushCalls: Array<{ state: unknown; url: string }> = [];
	readonly replaceCalls: Array<{ state: unknown; url: string }> = [];
	backCalls = 0;
	private entries: Array<{ state: unknown; url: string }>;
	private currentIndex = 0;
	private readonly listeners = new Set<EventListener>();

	readonly dependencies: WorkspaceRouteHistoryDependencies = {
		history: this,
		location: this.location,
		events: this,
	};

	constructor(initialUrl: string) {
		this.entries = [{ state: null, url: initialUrl }];
		this.syncLocation(initialUrl);
	}

	get state(): unknown {
		return this.entries[this.currentIndex].state;
	}

	get currentUrl(): string {
		return this.entries[this.currentIndex].url;
	}

	get length(): number {
		return this.entries.length;
	}

	get index(): number {
		return this.currentIndex;
	}

	pushState(state: unknown, _unused: string, url?: string | URL | null): void {
		const nextUrl = String(url ?? this.currentUrl);
		this.entries.splice(this.currentIndex + 1);
		this.entries.push({ state, url: nextUrl });
		this.currentIndex += 1;
		this.pushCalls.push({ state, url: nextUrl });
		this.syncLocation(nextUrl);
	}

	replaceState(state: unknown, _unused: string, url?: string | URL | null): void {
		const nextUrl = String(url ?? this.currentUrl);
		this.entries[this.currentIndex] = { state, url: nextUrl };
		this.replaceCalls.push({ state, url: nextUrl });
		this.syncLocation(nextUrl);
	}

	back(): void {
		this.backCalls += 1;
		if (this.currentIndex === 0) return;
		this.currentIndex -= 1;
		this.syncLocation(this.currentUrl);
		this.emitPopstate();
	}

	forward(): void {
		if (this.currentIndex >= this.entries.length - 1) return;
		this.currentIndex += 1;
		this.syncLocation(this.currentUrl);
		this.emitPopstate();
	}

	externalReplace(url: string): void {
		this.entries[this.currentIndex] = { state: null, url };
		this.syncLocation(url);
	}

	addEventListener(type: "popstate", listener: EventListener): void {
		if (type === "popstate") this.listeners.add(listener);
	}

	removeEventListener(type: "popstate", listener: EventListener): void {
		if (type === "popstate") this.listeners.delete(listener);
	}

	private syncLocation(url: string): void {
		const parsed = splitUrl(url);
		this.location.pathname = parsed.pathname;
		this.location.search = parsed.search;
		this.location.hash = parsed.hash;
	}

	private emitPopstate(): void {
		const event = { type: "popstate" } as Event;
		for (const listener of this.listeners) listener(event);
	}
}

function splitUrl(url: string): Required<WorkspaceRouteLocation> {
	const hashOffset = url.indexOf("#");
	const withoutHash = hashOffset === -1 ? url : url.slice(0, hashOffset);
	const hash = hashOffset === -1 ? "" : url.slice(hashOffset);
	const searchOffset = withoutHash.indexOf("?");
	return {
		pathname: searchOffset === -1 ? withoutHash : withoutHash.slice(0, searchOffset),
		search: searchOffset === -1 ? "" : withoutHash.slice(searchOffset),
		hash,
	};
}

function expectRoute(result: WorkspaceRouteParseResult) {
	expect(result.kind).toBe("route");
	return result as Extract<WorkspaceRouteParseResult, { kind: "route" }>;
}

function expectExecution(route: ReturnType<typeof expectRoute>["route"]): ExecutionRoute {
	expect(route.destination).toBe("execution");
	return route as ExecutionRoute;
}
