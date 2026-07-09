// @vitest-environment jsdom

import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeAll, describe, expect, it, vi } from "vitest";
import type { AgentApi } from "./agentApi.ts";
import { App } from "./App.tsx";
import type { ConnectionStatus } from "./rpc.ts";
import type {
	DelegationListResult,
	EventFrame,
	Project,
	SessionSnapshot,
	SessionSummary,
	TranscriptEntry,
	TranscriptTreeIndex,
	TranscriptTreeNode,
	TranscriptTurnsResult,
} from "./types.ts";
import { loadUiSelection, rememberUiSelection } from "./uiResume.ts";
import {
	WorkspaceRouteHistory,
	type WorkspaceHistoryLike,
	type WorkspacePopstateSource,
	type WorkspaceRouteHistoryDependencies,
	type WorkspaceRouteLocation,
} from "./workspaceRoute.ts";

beforeAll(() => {
	class ResizeObserver {
		observe() {}
		unobserve() {}
		disconnect() {}
	}
	vi.stubGlobal("ResizeObserver", ResizeObserver);
	HTMLElement.prototype.scrollIntoView ??= () => {};
	HTMLElement.prototype.hasPointerCapture ??= () => false;
	HTMLElement.prototype.setPointerCapture ??= () => {};
	HTMLElement.prototype.releasePointerCapture ??= () => {};
	vi.stubGlobal("requestAnimationFrame", (callback: FrameRequestCallback) =>
		window.setTimeout(() => callback(performance.now()), 0));
	vi.stubGlobal("cancelAnimationFrame", (handle: number) => window.clearTimeout(handle));
	vi.stubGlobal("matchMedia", (query: string) => ({
		matches: query === "(min-width: 1280px)",
		media: query,
		onchange: null,
		addEventListener: vi.fn(),
		removeEventListener: vi.fn(),
		addListener: vi.fn(),
		removeListener: vi.fn(),
		dispatchEvent: vi.fn(() => true),
	}));
});

afterEach(() => {
	cleanup();
	window.localStorage.clear();
	window.history.replaceState(null, "", "/");
});

describe("App workspace route identity integration", () => {
	it.each([
		{
			name: "wrong project",
			url: "/w/project/project-1/run/root-other/conversation/root-other",
			deferredSessionId: "root-other",
			result: snapshot("root-other", null, "project-2", "Other project root"),
		},
		{
			name: "wrong root",
			url: "/w/project/project-1/run/project-root-1/conversation/project-wrong-root-child",
			deferredSessionId: "project-wrong-root-child",
			result: snapshot(
				"project-wrong-root-child",
				"project-root-2",
				"project-1",
				"Wrong root child",
			),
		},
		{
			name: "child used as root",
			url: "/w/host/run/child-1/conversation/child-1",
			deferredSessionId: "child-1",
			result: snapshot("child-1", "root-1", null, "Child one"),
		},
	])("never starts non-validation reads for a rejected $name route", async ({
		url,
		deferredSessionId,
		result,
	}) => {
		const validation = deferred<SessionSnapshot>();
		const browser = new FakeWorkspaceBrowser(url);
		const api = createRouteApi({
			deferredSessions: new Map([[deferredSessionId, validation.promise]]),
		});
		const mounted = renderRouteApp(api, browser);

		await openStatusOnly(api);
		await waitFor(() =>
			expect(
				api.getSession.mock.calls.some(([sessionId]) => sessionId === deferredSessionId),
			).toBe(true),
		);
		expectSensitiveReads(api, 0);

		await act(async () => validation.resolve(result));
		expect(await screen.findByRole("heading", { name: /Couldn’t (load session|open this workspace)/ })).toBeTruthy();
		expectSensitiveReads(api, 0);

		await mounted.dispose();
	});

	it("starts session, transcript, tools, events, delegations, and project session reads only after validation", async () => {
		const validation = deferred<SessionSnapshot>();
		const browser = new FakeWorkspaceBrowser(
			"/w/project/project-1/run/project-root-1/conversation/project-child-1",
		);
		const api = createRouteApi({
			deferredSessions: new Map([["project-child-1", validation.promise]]),
		});
		const mounted = renderRouteApp(api, browser);

		await openStatusOnly(api);
		await waitFor(() =>
			expect(api.getSession.mock.calls.some(([sessionId]) => sessionId === "project-child-1")).toBe(true),
		);
		expectSensitiveReads(api, 0);

		await act(async () =>
			validation.resolve(snapshot(
				"project-child-1",
				"project-root-1",
				"project-1",
				"Project child",
			)),
		);
		await waitFor(() => {
			expect(api.listSessions).toHaveBeenCalledWith(100, "project-1");
			expect(api.getTranscriptTurns).toHaveBeenCalledWith("project-child-1", { limit: 50 });
			expect(api.listTools).toHaveBeenCalled();
			expect(api.subscribeEvents).toHaveBeenCalled();
			expect(api.listDelegations).toHaveBeenCalledWith("project-root-1", 3);
		});

		await mounted.dispose();
	});

	it("keeps Back/Forward target reads fenced across a stale validation completion", async () => {
		const firstValidation = deferred<SessionSnapshot>();
		const forwardValidation = deferred<SessionSnapshot>();
		const deferredSessions = new Map<string, Promise<SessionSnapshot>>([
			["child-a", firstValidation.promise],
		]);
		const browser = new FakeWorkspaceBrowser("/w/host/run/root-1/conversation/root-1");
		const api = createRouteApi({ deferredSessions });
		const mounted = renderRouteApp(api, browser);

		await open(api);
		await act(async () => browser.navigate("/w/host/run/root-1/conversation/child-a"));
		await waitFor(() =>
			expect(api.getSession.mock.calls.some(([sessionId]) => sessionId === "child-a")).toBe(true),
		);
		expect(api.getTranscriptTurns.mock.calls.some(([sessionId]) => sessionId === "child-a")).toBe(false);
		expect(api.subscribeEvents.mock.calls.some(([sessionId]) => sessionId === "child-a")).toBe(false);

		deferredSessions.set("child-a", forwardValidation.promise);
		await act(async () => browser.back());
		await waitFor(() =>
			expect(browser.currentUrl).toBe("/w/host/run/root-1/conversation/root-1"),
		);
		await act(async () =>
			firstValidation.resolve(snapshot("child-a", "root-1", null, "Stale child")),
		);
		expect(api.getTranscriptTurns.mock.calls.some(([sessionId]) => sessionId === "child-a")).toBe(false);

		await act(async () => browser.forward());
		await waitFor(() =>
			expect(api.getSession.mock.calls.filter(([sessionId]) => sessionId === "child-a")).toHaveLength(2),
		);
		expect(api.getTranscriptTurns.mock.calls.some(([sessionId]) => sessionId === "child-a")).toBe(false);
		await act(async () =>
			forwardValidation.resolve(snapshot("child-a", "root-1", null, "Current child")),
		);
		await waitFor(() =>
			expect(api.getTranscriptTurns.mock.calls.some(([sessionId]) => sessionId === "child-a")).toBe(true),
		);

		await mounted.dispose();
	});

	it("lets a direct child URL beat localStorage, pins Agents to root, and loads the child conversation", async () => {
		rememberLegacy("legacy-root");
		const browser = new FakeWorkspaceBrowser("/w/host/run/root-1/conversation/child-1");
		const api = createRouteApi();
		const mounted = renderRouteApp(api, browser);

		await openStatusOnly(api);
		await waitFor(() =>
			expect(screen.queryByText("Loading conversation")).toBeNull(),
		);
		await waitFor(() =>
			expect(document.querySelector(".log-session")?.textContent).toBe("Child one"),
		);
		const normalChildChrome = [
			document.querySelector(".topbar"),
			document.querySelector(".log-header"),
		].filter((element): element is Element => element !== null);
		for (const element of normalChildChrome) {
			expect(element.textContent).not.toContain("child-1");
			expect(element.textContent).not.toContain("root-1");
			for (const labelledElement of element.querySelectorAll("[aria-label], [title]")) {
				expect(labelledElement.getAttribute("aria-label") ?? "").not.toMatch(/child-1|root-1/);
				expect(labelledElement.getAttribute("title") ?? "").not.toMatch(/child-1|root-1/);
			}
		}
		const parentLinks = screen.getAllByRole("button", { name: "parent" });
		expect(parentLinks).toHaveLength(2);
		for (const parentLink of parentLinks) {
			expect(parentLink.getAttribute("title")).toBe("Open parent conversation");
		}
		expect(api.getTranscriptTurns).toHaveBeenCalledWith("child-1", { limit: 50 });
		expect(api.listDelegations).toHaveBeenCalledWith("root-1", 3);
		expect(api.listDelegations.mock.calls.every(([parent]) => parent === "root-1")).toBe(true);
		expect(browser.currentUrl).toBe("/w/host/run/root-1/conversation/child-1");
		expect(loadUiSelection()).toEqual({ projectId: null, sessionId: null });

		await mounted.dispose();
	});

	it.each([
		{ selected: "root-1", expected: "/w/host/run/root-1/conversation/root-1" },
		{ selected: "child-1", expected: "/w/host/run/root-1/conversation/child-1" },
	])("migrates legacy $selected after resolving its canonical direct parent", async ({ selected, expected }) => {
		rememberLegacy(selected);
		const browser = new FakeWorkspaceBrowser("/");
		const api = createRouteApi();
		const mounted = renderRouteApp(api, browser);

		await open(api);
		await waitFor(() => expect(browser.currentUrl).toBe(expected));
		expect(browser.replaceCalls).toHaveLength(1);
		expect(loadUiSelection()).toEqual({ projectId: null, sessionId: null });
		const log = document.querySelector(".log-pane");
		expect(log?.textContent).toContain(selected === "root-1" ? "Root one" : "Child one");

		await mounted.dispose();
	});

	it("pushes child/parent Conversation navigation and restores both identities atomically with Back/Forward", async () => {
		const clientHeightSpy = vi.spyOn(HTMLElement.prototype, "clientHeight", "get").mockImplementation(function (this: HTMLElement) {
			return this.classList.contains("message-scroll") ? 100 : 0;
		});
		const scrollHeightSpy = vi.spyOn(HTMLElement.prototype, "scrollHeight", "get").mockImplementation(function (this: HTMLElement) {
			return this.classList.contains("message-scroll") ? 1000 : 0;
		});
		const browser = new FakeWorkspaceBrowser("/w/host/run/root-1/conversation/root-1");
		const api = createRouteApi();
		const mounted = renderRouteApp(api, browser);
		const user = userEvent.setup();

		await open(api);
		expect(document.querySelector<HTMLDivElement>(".message-scroll")?.scrollTop).toBe(900);
		document.querySelector<HTMLDivElement>(".message-scroll")!.scrollTop = 137;
		fireEvent.scroll(document.querySelector<HTMLDivElement>(".message-scroll")!);
		const child = await screen.findByRole("button", { name: /Open agent (Child one|Agent), implementer/ });
		await user.click(child);
		await waitFor(() => expect(browser.currentUrl).toBe("/w/host/run/root-1/conversation/child-1"));
		await waitFor(() => expect(document.querySelector(".log-session")?.textContent).toBe("Child one"));
		expect(document.querySelector<HTMLDivElement>(".message-scroll")?.scrollTop).toBe(900);
		expect(api.listDelegations.mock.calls.at(-1)?.[0]).toBe("root-1");

		document.querySelector<HTMLDivElement>(".message-scroll")!.scrollTop = 211;
		fireEvent.scroll(document.querySelector<HTMLDivElement>(".message-scroll")!);
		const parentLinks = screen.getAllByRole("button", { name: "parent" });
		await user.click(parentLinks[0]);
		await waitFor(() => expect(browser.currentUrl).toBe("/w/host/run/root-1/conversation/root-1"));
		expect(document.querySelector<HTMLDivElement>(".message-scroll")?.scrollTop).toBe(900);

		const mutationsBeforePop = mutationCallCount(api);
		const pushesBeforePop = browser.pushCalls.length;
		const replacesBeforePop = browser.replaceCalls.length;
		document.querySelector<HTMLDivElement>(".message-scroll")!.scrollTop = 315;
		fireEvent.scroll(document.querySelector<HTMLDivElement>(".message-scroll")!);
		await act(async () => browser.back());
		await waitFor(() => expect(document.querySelector(".log-session")?.textContent).toBe("Child one"));
		expect(browser.currentUrl).toBe("/w/host/run/root-1/conversation/child-1");
		expect(document.querySelector<HTMLDivElement>(".message-scroll")?.scrollTop).toBe(900);
		document.querySelector<HTMLDivElement>(".message-scroll")!.scrollTop = 417;
		fireEvent.scroll(document.querySelector<HTMLDivElement>(".message-scroll")!);
		await act(async () => browser.forward());
		await waitFor(() => expect(browser.currentUrl).toBe("/w/host/run/root-1/conversation/root-1"));
		expect(document.querySelector<HTMLDivElement>(".message-scroll")?.scrollTop).toBe(900);
		expect(mutationCallCount(api)).toBe(mutationsBeforePop);
		expect(browser.pushCalls).toHaveLength(pushesBeforePop);
		expect(browser.replaceCalls).toHaveLength(replacesBeforePop);

		await mounted.dispose();
		clientHeightSpy.mockRestore();
		scrollHeightSpy.mockRestore();
	});

	it("owns invalid required Conversation, project mismatch, and malformed detail states", async () => {
		const missingBrowser = new FakeWorkspaceBrowser("/w/host/run/root-1/conversation/missing-child");
		const missingApi = createRouteApi({ missingSessionIds: new Set(["missing-child"]) });
		let mounted = renderRouteApp(missingApi, missingBrowser);
		await open(missingApi);
		expect(await screen.findByRole("heading", { name: "Couldn’t load session" })).toBeTruthy();
		expect(screen.getByRole("button", { name: "Open root Conversation" })).toBeTruthy();
		expect(screen.queryByRole("textbox")).toBeNull();
		expect(missingBrowser.replaceCalls).toHaveLength(0);
		await mounted.dispose();

		const mismatchBrowser = new FakeWorkspaceBrowser(
			"/w/project/project-1/run/root-other/conversation/root-other",
		);
		const mismatchApi = createRouteApi();
		mounted = renderRouteApp(mismatchApi, mismatchBrowser);
		await open(mismatchApi);
		expect(await screen.findByText(/belongs to project project-2, not project project-1/)).toBeTruthy();
		expect(screen.queryByRole("textbox")).toBeNull();
		await mounted.dispose();

		const detailBrowser = new FakeWorkspaceBrowser(
			"/w/host/run/root-1/execution/activity?focus=delegation%3Awork-1",
		);
		const detailApi = createRouteApi();
		mounted = renderRouteApp(detailApi, detailBrowser);
		await open(detailApi);
		expect(await screen.findByRole("heading", { name: "Couldn’t open this workspace" })).toBeTruthy();
		expect(screen.getByRole("alert").textContent).toContain("unbounded canonical delegation lookup");
		expect(screen.getByRole("button", { name: "Back to root Outline" })).toBeTruthy();
		expect(mutationCallCount(detailApi)).toBe(0);
		await mounted.dispose();
	});

	it("falls back an unavailable optional Execution conversation visibly and hides the composer", async () => {
		const browser = new FakeWorkspaceBrowser(
			"/w/host/run/root-1/execution/activity?conversation=agent%3Amissing-child",
		);
		const api = createRouteApi({ missingSessionIds: new Set(["missing-child"]) });
		const mounted = renderRouteApp(api, browser);
		const user = userEvent.setup();

		await open(api);
		expect(await screen.findByRole("heading", { name: /Execution workspace is not available/ })).toBeTruthy();
		expect(screen.getByRole("alert").textContent).toContain("requested conversation was unavailable");
		expect(browser.currentUrl).toBe("/w/host/run/root-1/execution/activity");
		expect(browser.replaceCalls).toHaveLength(1);
		expect(screen.queryByRole("textbox")).toBeNull();
		expect(document.querySelector("[data-slot='execution-placeholder']")?.textContent).toContain(
			"Conversation root-1",
		);

		await user.click(screen.getByRole("button", { name: "Open effective Conversation" }));
		await waitFor(() => expect(browser.currentUrl).toBe("/w/host/run/root-1/conversation/root-1"));
		expect(await screen.findByRole("textbox")).toBeTruthy();

		await mounted.dispose();
	});

	it("preserves child drafts across Execution and re-enters the effective Conversation at latest", async () => {
		const clientHeightSpy = vi.spyOn(HTMLElement.prototype, "clientHeight", "get").mockImplementation(function (this: HTMLElement) {
			return this.classList.contains("message-scroll") ? 100 : 0;
		});
		const scrollHeightSpy = vi.spyOn(HTMLElement.prototype, "scrollHeight", "get").mockImplementation(function (this: HTMLElement) {
			return this.classList.contains("message-scroll") ? 1000 : 0;
		});
		const browser = new FakeWorkspaceBrowser("/w/host/run/root-1/conversation/child-1");
		const api = createRouteApi();
		const mounted = renderRouteApp(api, browser);
		const user = userEvent.setup();

		await open(api);
		const composer = await screen.findByRole<HTMLTextAreaElement>("textbox");
		await user.type(composer, "child draft survives");
		const scroller = document.querySelector<HTMLDivElement>(".message-scroll");
		if (!scroller) throw new Error("missing transcript scroller");
		scroller.scrollTop = 137;
		fireEvent.scroll(scroller);
		await act(async () => {
			browser.navigate(
				"/w/host/run/root-1/execution/overview?conversation=agent%3Achild-1",
			);
		});
		expect(await screen.findByRole("heading", { name: /Execution workspace is not available/ })).toBeTruthy();
		expect(screen.queryByRole("textbox")).toBeNull();
		await user.click(screen.getByRole("button", { name: "Open effective Conversation" }));
		const restored = await screen.findByRole<HTMLTextAreaElement>("textbox");
		expect(restored.value).toBe("child draft survives");
		const restoredScroller = document.querySelector<HTMLDivElement>(".message-scroll");
		expect(restoredScroller?.scrollTop).toBe(900);
		expect(api.getTranscriptTurns.mock.calls.filter(([sessionId]) => sessionId === "child-1")).toHaveLength(1);

		await mounted.dispose();
		clientHeightSpy.mockRestore();
		scrollHeightSpy.mockRestore();
	});

	it("fences stale validation and keeps in-flight composer targets immutable across popstate", async () => {
		const stale = deferred<SessionSnapshot>();
		const steer = deferred<{ accepted: true }>();
		const browser = new FakeWorkspaceBrowser("/w/host/run/root-1/conversation/child-a");
		const api = createRouteApi({ deferredSessions: new Map([["child-a", stale.promise]]) });
		(api.steerSubagent as ApiSpy).mockImplementation(() => steer.promise);
		const mounted = renderRouteApp(api, browser);
		await openStatusOnly(api);

		await act(async () => browser.navigate("/w/host/run/root-1/conversation/child-1"));
		await waitFor(() => expect(document.querySelector(".log-session")?.textContent).toBe("Child one"));
		stale.resolve(snapshot("child-a", "root-1", null, "Stale child"));
		await act(async () => {
			await stale.promise;
			await Promise.resolve();
		});
		expect(browser.currentUrl).toBe("/w/host/run/root-1/conversation/child-1");
		expect(document.querySelector(".log-session")?.textContent).toBe("Child one");

		const composer = await screen.findByRole<HTMLTextAreaElement>("textbox");
		await userEvent.type(composer, "captured child message");
		await userEvent.click(screen.getByRole("button", { name: "send message" }));
		await waitFor(() => expect(api.steerSubagent).toHaveBeenCalled());
		await act(async () => browser.navigate("/w/host/run/root-1/conversation/root-1"));
		expect((api.steerSubagent as ApiSpy).mock.calls[0][0]).toMatchObject({
			parentSessionId: "root-1",
			subagentSessionId: "child-1",
			message: "captured child message",
		});
		steer.resolve({ accepted: true });

		await mounted.dispose();
	});

	it("preserves a project new-session draft and workspace scope across remount, then starts in the original project", async () => {
		const browser = new FakeWorkspaceBrowser("/");
		let api = createRouteApi();
		let mounted = renderRouteApp(api, browser);
		const user = userEvent.setup();

		await open(api);
		const projectButton = screen
			.getAllByRole("button", { name: /Project one/ })
			.find((button) => button.classList.contains("project-row-primary"));
		if (!projectButton) throw new Error("missing Project one selector");
		await user.click(projectButton);
		await waitFor(() => expect(loadUiSelection()).toEqual({ projectId: "project-1", sessionId: null }));
		const composer = await screen.findByRole<HTMLTextAreaElement>("textbox");
		await user.type(composer, "start a scoped project root");
		await user.click(screen.getByRole("button", { name: /Workspaces/ }));
		await user.click(screen.getByRole("checkbox", { name: /docs/ }));
		await user.type(screen.getByRole("textbox", { name: "branch for repo-a" }), "feature/refresh");
		expect(browser.currentUrl).toBe("/");

		await mounted.dispose();

		api = createRouteApi();
		api.startSession.mockImplementation(async (params: { sessionId: string }) => ({
			session_id: params.sessionId,
			activity: "queued",
		}));
		mounted = renderRouteApp(api, browser);

		await open(api);
		const restored = await screen.findByRole<HTMLTextAreaElement>("textbox");
		expect(restored.value).toBe("start a scoped project root");
		expect(
			screen
				.getAllByRole("button", { name: /Project one/ })
				.find((button) => button.classList.contains("project-row-primary"))
				?.getAttribute("aria-current"),
		).toBe("page");
		await user.click(screen.getByRole("button", { name: /Workspaces/ }));
		expect(screen.getByRole<HTMLInputElement>("checkbox", { name: /docs/ }).checked).toBe(false);
		expect(screen.getByRole<HTMLInputElement>("textbox", { name: "branch for repo-a" }).value).toBe(
			"feature/refresh",
		);
		expect(browser.currentUrl).toBe("/");
		await user.click(screen.getByRole("button", { name: "send message" }));
		await waitFor(() => expect(api.startSession).toHaveBeenCalled());
		const params = api.startSession.mock.calls[0][0];
		const createdId = params.sessionId;
		expect(params).toMatchObject({
			projectId: "project-1",
			workspaces: [{ workspaceDir: "repo-a", branch: "feature/refresh" }],
		});
		await waitFor(() =>
			expect(browser.currentUrl).toBe(
				`/w/project/project-1/run/${createdId}/conversation/${createdId}`,
			));

		await mounted.dispose();
	});

	it("keeps history switching bound to the dialog session across a popstate during restore", async () => {
		const restoredEntry = deferred<{
			session_id: string;
			session_revision: number;
			transcript_revision: number;
			entries: TranscriptEntry[];
		}>();
		const browser = new FakeWorkspaceBrowser("/w/host/run/root-1/conversation/root-1");
		const api = createRouteApi({
			historySessionIds: new Set(["root-1"]),
			deferredTranscriptEntries: restoredEntry.promise,
		});
		api.switchHistory.mockImplementation(async (params: { sessionId: string; leafId: string | null }) => ({
			session_id: params.sessionId,
			active_leaf_id: params.leafId,
		}));
		const mounted = renderRouteApp(api, browser);
		const user = userEvent.setup();

		await open(api);
		const composer = await screen.findByRole<HTMLTextAreaElement>("textbox");
		await user.type(composer, "/switch");
		await user.click(screen.getByRole("button", { name: "send message" }));
		const target = await screen.findByRole("button", { name: /Switch to User message/ });
		await user.click(target);
		await waitFor(() => expect(api.getTranscriptEntries).toHaveBeenCalledWith("root-1", ["entry-user"]));

		const pushesBeforePop = browser.pushCalls.length;
		const replacesBeforePop = browser.replaceCalls.length;
		await act(async () => browser.popstate("/w/host/run/root-1/conversation/child-1"));
		expect(screen.queryByRole("dialog")).toBeNull();
		restoredEntry.resolve({
			session_id: "root-1",
			session_revision: 1,
			transcript_revision: 1,
			entries: [userMessageEntry()],
		});
		await waitFor(() => expect(api.switchHistory).toHaveBeenCalledTimes(1));
		expect(api.switchHistory.mock.calls[0][0]).toMatchObject({
			sessionId: "root-1",
			leafId: null,
			expectedActiveLeafId: "entry-active",
		});
		expect(browser.currentUrl).toBe("/w/host/run/root-1/conversation/child-1");
		expect(browser.pushCalls).toHaveLength(pushesBeforePop);
		expect(browser.replaceCalls).toHaveLength(replacesBeforePop);

		await mounted.dispose();
	});

	it("abandons a pending history destination when the route selects another conversation", async () => {
		const clientHeightSpy = vi.spyOn(HTMLElement.prototype, "clientHeight", "get").mockImplementation(function (this: HTMLElement) {
			return this.classList.contains("message-scroll") ? 100 : 0;
		});
		const scrollHeightSpy = vi.spyOn(HTMLElement.prototype, "scrollHeight", "get").mockImplementation(function (this: HTMLElement) {
			return this.classList.contains("message-scroll") ? 1000 : 0;
		});
		const browser = new FakeWorkspaceBrowser("/w/host/run/root-1/conversation/root-1");
		const api = createRouteApi({
			historySessionIds: new Set(["root-1"]),
			includeDestinationHistoryTarget: true,
		});
		api.getTranscriptTurns.mockResolvedValue(
			turnsWithContent("root-1", "entry-active", "old rendered page", 1),
		);
		const mounted = renderRouteApp(api, browser);
		const user = userEvent.setup();
		await open(api);
		expect(await screen.findByText("old rendered page")).toBeTruthy();

		const oldRefresh = deferred<SessionSnapshot>();
		const destinationTurns = deferred<TranscriptTurnsResult>();
		api.getSession
			.mockImplementationOnce(() => oldRefresh.promise)
			.mockImplementationOnce(async () => ({
				...snapshot("root-1", null, null, "Root one", "entry-destination"),
				session_revision: 2,
				transcript_revision: 2,
				last_event_id: 3,
			}));
		api.getTranscriptTurns.mockImplementationOnce(() => destinationTurns.promise);
		api.emitEvent({
			event_id: 2,
			event: "session.configured",
			session_id: "root-1",
			data: {},
		});
		await waitFor(() =>
			expect(api.getSession.mock.results.some(({ value }) => value === oldRefresh.promise)).toBe(true),
		);

		api.switchHistory.mockImplementation(async () => ({
			session_id: "root-1",
			active_leaf_id: "entry-destination",
			session_revision: 2,
			transcript_revision: 2,
			last_event_id: 3,
		}));
		await user.type(screen.getByRole("textbox"), "/switch");
		await user.click(screen.getByRole("button", { name: "send message" }));
		await user.click(await screen.findByRole("button", { name: /Switch to.*Destination answer/ }));
		await waitFor(() =>
			expect(api.getTranscriptTurns.mock.results.some(({ value }) => value === destinationTurns.promise)).toBe(true),
		);
		expect(screen.getByText("old rendered page")).toBeTruthy();

		await act(async () => {
			browser.navigate("/w/host/run/root-1/conversation/child-1");
		});
		await waitFor(() => {
			expect(browser.currentUrl).toBe("/w/host/run/root-1/conversation/child-1");
			expect(document.querySelector(".log-pane")?.textContent).toContain("Child one");
			expect(api.getTranscriptTurns.mock.calls.some(([sessionId]) => sessionId === "child-1")).toBe(true);
		});
		const childScroller = document.querySelector<HTMLDivElement>(".message-scroll");
		if (!childScroller) throw new Error("missing child transcript scroller");
		childScroller.scrollTop = 321;
		fireEvent.scroll(childScroller);

		await act(async () => {
			destinationTurns.resolve(
				turnsWithContent(
					"root-1",
					"entry-destination",
					"abandoned destination page",
					2,
				),
			);
			oldRefresh.resolve(snapshot("root-1", null, null, "Root one", "entry-active"));
			await Promise.all([destinationTurns.promise, oldRefresh.promise]);
		});
		expect(document.querySelector(".log-pane")?.textContent).toContain("Child one");
		expect(screen.queryByText("abandoned destination page")).toBeNull();
		expect(childScroller.scrollTop).toBe(321);

		await mounted.dispose();
		clientHeightSpy.mockRestore();
		scrollHeightSpy.mockRestore();
	});

});

type ApiSpy = ReturnType<typeof vi.fn>;

type RouteApi = AgentApi & {
	getSession: ApiSpy;
	getTranscriptEntries: ApiSpy;
	getTranscriptTurns: ApiSpy;
	listSessions: ApiSpy;
	listTools: ApiSpy;
	listDelegations: ApiSpy;
	subscribeEvents: ApiSpy;
	queueFollowUp: ApiSpy;
	startSession: ApiSpy;
	switchHistory: ApiSpy;
	emitStatus(status: ConnectionStatus): void;
	emitEvent(event: EventFrame): void;
};

function createRouteApi(
	options: {
		missingSessionIds?: Set<string>;
		deferredSessions?: Map<string, Promise<SessionSnapshot>>;
		historySessionIds?: Set<string>;
		includeDestinationHistoryTarget?: boolean;
		activeLeafIds?: Map<string, string | null>;
		deferredTranscriptTurns?: Map<string, Promise<TranscriptTurnsResult>>;
		deferredTranscriptEntries?: Promise<{
			session_id: string;
			session_revision: number;
			transcript_revision: number;
			entries: TranscriptEntry[];
		}>;
	} = {},
): RouteApi {
	let open = false;
	const statusListeners = new Set<(status: ConnectionStatus) => void>();
	const eventListeners = new Set<(event: EventFrame) => void>();
	const summaries = [
		summary("root-1", null, null, "Root one"),
		summary("legacy-root", null, null, "Legacy root"),
		summary("project-root-1", null, "project-1", "Project root"),
		summary("project-child-1", "project-root-1", "project-1", "Project child"),
		summary("root-other", null, "project-2", "Other project root"),
	];
	const projects: Project[] = [
		{
			project_id: "project-1",
			name: "Project one",
			workspaces: [
				{
					kind: "git",
					workspace_dir: "repo-a",
					remote_url: "https://example.test/repo-a.git",
					remote_branch: "main",
				},
				{
					kind: "local",
					workspace_dir: "docs",
					source_path: "/srv/docs",
				},
			],
			metadata: {},
			created_at: "2026-01-01T00:00:00Z",
			updated_at: "2026-01-01T00:00:00Z",
		},
		{
			project_id: "project-2",
			name: "Project two",
			workspaces: [],
			metadata: {},
			created_at: "2026-01-01T00:00:00Z",
			updated_at: "2026-01-01T00:00:00Z",
		},
	];
	const getSession = vi.fn(async (sessionId: string) => {
		const deferred = options.deferredSessions?.get(sessionId);
		if (deferred) return deferred;
		if (options.missingSessionIds?.has(sessionId)) throw new Error("session not found");
		if (sessionId === "root-1") {
			return snapshot(
				"root-1",
				null,
				null,
				"Root one",
				options.activeLeafIds?.get(sessionId) ??
					(options.historySessionIds?.has(sessionId) ? "entry-active" : null),
			);
		}
		if (sessionId === "child-1") return snapshot("child-1", "root-1", null, "Child one");
		if (sessionId === "child-a") return snapshot("child-a", "root-1", null, "Child A");
		if (sessionId === "legacy-root") return snapshot("legacy-root", null, null, "Legacy root");
		if (sessionId === "project-root-1") {
			return snapshot("project-root-1", null, "project-1", "Project root");
		}
		if (sessionId === "project-child-1") {
			return snapshot("project-child-1", "project-root-1", "project-1", "Project child");
		}
		if (sessionId === "project-wrong-root-child") {
			return snapshot(
				"project-wrong-root-child",
				"project-root-2",
				"project-1",
				"Wrong root child",
			);
		}
		if (sessionId === "root-other") return snapshot("root-other", null, "project-2", "Other project root");
		throw new Error(`session not found: ${sessionId}`);
	});
	const listDelegations = vi.fn(async (parentSessionId: string): Promise<DelegationListResult> => ({
		parent_session_id: parentSessionId,
		has_more: false,
		delegations: parentSessionId === "root-1"
			? [{
				delegation_id: "delegation-1",
				kind: "full",
				status: "running",
				workflow: null,
				label: "Child work",
				progress: { expected: 1, spawned: 1, terminal: 0, running: 1, failed: 0 },
				subagents: [{
					id: "child-1",
					status: "running",
					activity: "running",
					role: "implementer",
					subagent_type: "full",
				}],
			}]
			: [],
	}));
	const mutation = () => vi.fn(async () => {
		throw new Error("unexpected mutation");
	});
	const api = {
		connect: vi.fn(async () => undefined),
		reconnect: vi.fn(async () => undefined),
		close: vi.fn(),
		isOpen: () => open,
		onStatus: (listener: (status: ConnectionStatus) => void) => {
			statusListeners.add(listener);
			return () => statusListeners.delete(listener);
		},
		onEvent: (listener: (event: EventFrame) => void) => {
			eventListeners.add(listener);
			return () => eventListeners.delete(listener);
		},
		listProjects: vi.fn(async () => projects),
		listSessions: vi.fn(async (_limit: number, projectId: string | null) =>
			summaries.filter((session) => session.project_id === projectId)),
		listDelegations,
		listTools: vi.fn(async () => []),
		getSession,
		getTranscriptTurns: vi.fn(async (sessionId: string) =>
			options.deferredTranscriptTurns?.get(sessionId) ??
			emptyTurns(
				sessionId,
				options.activeLeafIds?.get(sessionId) ??
					(options.historySessionIds?.has(sessionId) ? "entry-active" : null),
			)),
		subscribeEvents: vi.fn(async () => []),
		unsubscribeEvents: vi.fn(async () => undefined),
		queueFollowUp: mutation(),
		startSession: mutation(),
		steerSubagent: mutation(),
		interrupt: mutation(),
		configureSession: mutation(),
		renameSession: mutation(),
		deleteSession: mutation(),
		resumeTurn: mutation(),
		switchHistory: mutation(),
		promoteQueuedInput: mutation(),
		updateQueuedInput: mutation(),
		cancelQueuedInput: mutation(),
		reorderQueuedFollowUps: mutation(),
		requestCompaction: mutation(),
		startFullDelegation: mutation(),
		startReadonlyDelegationFanout: mutation(),
		cancelDelegation: mutation(),
		createProject: mutation(),
		updateProject: mutation(),
		deleteProject: mutation(),
		getTranscriptIndex: vi.fn(async (sessionId: string) =>
			options.historySessionIds?.has(sessionId)
				? historyIndex(sessionId, options.includeDestinationHistoryTarget)
				: Promise.reject(new Error("unexpected transcript index read"))),
		getTranscriptEntries: vi.fn(async () =>
			options.deferredTranscriptEntries ??
			Promise.reject(new Error("unexpected transcript entry read"))),
		getTranscriptTurnDetail: mutation(),
		getHistoryTree: mutation(),
		getHistoryContext: mutation(),
		getSystemPrompt: mutation(),
		syncActiveBranch: mutation(),
		readHandoffFile: mutation(),
		emitStatus(status: ConnectionStatus) {
			open = status === "open";
			for (const listener of statusListeners) listener(status);
		},
		emitEvent(event: EventFrame) {
			for (const listener of eventListeners) listener(event);
		},
	} as unknown as RouteApi;
	return api;
}

function renderRouteApp(api: RouteApi, browser: FakeWorkspaceBrowser) {
	const client = new QueryClient({
		defaultOptions: {
			queries: { retry: false, gcTime: Infinity, refetchOnWindowFocus: false },
			mutations: { retry: false },
		},
	});
	const result = render(
		<QueryClientProvider client={client}>
			<App api={api} routeHistory={new WorkspaceRouteHistory(browser.dependencies)} />
		</QueryClientProvider>,
	);
	return {
		...result,
		async dispose() {
			result.unmount();
			await client.cancelQueries();
			client.clear();
		},
	};
}

function nonEmptyTurns(sessionId: string): TranscriptTurnsResult {
	return turnsWithContent(sessionId, "entry-finish", "late routed content", 1);
}

function turnsWithContent(
	sessionId: string,
	activeLeafId: string,
	text: string,
	transcriptRevision: number,
): TranscriptTurnsResult {
	return {
		...emptyTurns(sessionId, activeLeafId),
		session_revision: transcriptRevision,
		transcript_revision: transcriptRevision,
		cards: [{
			id: activeLeafId,
			turn_id: 1,
			status: "completed",
			outcome: "Graceful",
			start_entry_id: `${activeLeafId}-start`,
			boundary_entry_id: activeLeafId,
			active_leaf_id: activeLeafId,
			start_sequence: 1,
			end_sequence: 3,
			start_timestamp_ms: 1,
			timestamp_ms: 3,
			user_messages: [{
				id: `${activeLeafId}-user`,
				parent_id: `${activeLeafId}-start`,
				timestamp_ms: 2,
				item: {
					type: "user_message",
					content: [{ type: "text", text }],
				},
			}],
			assistant_message: null,
			summary: null,
			can_resume: false,
		}],
	};
}

async function open(api: RouteApi) {
	await openStatusOnly(api);
	await waitFor(() => expect(screen.queryByText("Loading conversation")).toBeNull());
}

async function openStatusOnly(api: RouteApi) {
	await act(async () => {
		api.emitStatus("open");
		await Promise.resolve();
	});
}

function summary(
	sessionId: string,
	parentSessionId: string | null,
	projectId: string | null,
	title: string,
): SessionSummary {
	return {
		session_id: sessionId,
		project_id: projectId,
		parent_session_id: parentSessionId,
		outer_cwd: "/workspace",
		workspaces: [],
		activity: "idle",
		active_leaf_id: null,
		provider: { kind: "openai", model: "gpt-5.1" },
		metadata: { title },
		created_at: "2026-01-01T00:00:00Z",
		updated_at: "2026-01-01T00:00:01Z",
		has_transcript_entries: false,
	};
}

function snapshot(
	sessionId: string,
	parentSessionId: string | null,
	projectId: string | null,
	title: string,
	activeLeafId: string | null = null,
): SessionSnapshot {
	return {
		...summary(sessionId, parentSessionId, projectId, title),
		active_leaf_id: activeLeafId,
		has_transcript_entries: activeLeafId !== null,
		pending_actions: [],
		queued_inputs: [],
		session_revision: 1,
		queue_revision: 1,
		transcript_revision: 1,
		last_event_id: 1,
		server_time_ms: 1,
	};
}

function emptyTurns(sessionId: string, activeLeafId: string | null = null): TranscriptTurnsResult {
	return {
		session_id: sessionId,
		active_leaf_id: activeLeafId,
		session_revision: 1,
		transcript_revision: 1,
		before_entry_id: null,
		next_before_entry_id: null,
		has_more_before: false,
		limit: 50,
		cards: [],
	};
}

function rememberLegacy(sessionId: string) {
	rememberUiSelection(null, sessionId);
}

function expectSensitiveReads(api: RouteApi, count: number): void {
	const spies = [
		api.listSessions,
		api.getTranscriptTurns,
		api.listTools,
		api.subscribeEvents,
		api.listDelegations,
		api.getTranscriptEntries,
		api.getTranscriptIndex,
		api.getTranscriptTurnDetail,
		api.readHandoffFile,
	] as ApiSpy[];
	expect(
		spies.reduce((total, spy) => total + spy.mock.calls.length, 0),
	).toBe(count);
}

function historyIndex(
	sessionId: string,
	includeDestinationHistoryTarget = false,
): TranscriptTreeIndex {
	const destination = includeDestinationHistoryTarget
		? [historyNode(
				"entry-destination",
				"entry-user",
				3,
				"turn_finished",
				"Destination answer",
			)]
		: [];
	return {
		session_id: sessionId,
		active_leaf_id: "entry-active",
		session_revision: 1,
		transcript_revision: 1,
		after_sequence: 0,
		max_sequence: includeDestinationHistoryTarget ? 3 : 2,
		complete: true,
		nodes: [
			historyNode("entry-user", null, 1, "user_message", "Edit original prompt"),
			historyNode("entry-active", "entry-user", 2, "turn_finished", "Original answer"),
			...destination,
		],
	};
}

function historyNode(
	id: string,
	parentId: string | null,
	sequence: number,
	itemType: TranscriptTreeNode["item_type"],
	displayHint: string,
): TranscriptTreeNode {
	return {
		id,
		parent_id: parentId,
		timestamp_ms: sequence,
		sequence,
		item_type: itemType,
		turn_id: 1,
		outcome: itemType === "turn_finished" ? "Graceful" : null,
		can_switch_to: true,
		display_hint: displayHint,
	};
}

function userMessageEntry(): TranscriptEntry {
	return {
		id: "entry-user",
		parent_id: null,
		timestamp_ms: 1,
		sequence: 1,
		item: { type: "user_message", content: [{ type: "text", text: "Edit original prompt" }] },
	};
}

function mutationCallCount(api: RouteApi): number {
	return [
		api.queueFollowUp,
		api.startSession,
		api.steerSubagent,
		api.interrupt,
		api.configureSession,
		api.renameSession,
		api.deleteSession,
		api.resumeTurn,
		api.switchHistory,
	].reduce((total, candidate) => total + (candidate as ApiSpy).mock.calls.length, 0);
}

function deferred<T>() {
	let resolve!: (value: T) => void;
	let reject!: (reason?: unknown) => void;
	const promise = new Promise<T>((nextResolve, nextReject) => {
		resolve = nextResolve;
		reject = nextReject;
	});
	return { promise, resolve, reject };
}

class FakeWorkspaceBrowser implements WorkspaceHistoryLike, WorkspacePopstateSource {
	readonly location: WorkspaceRouteLocation = { pathname: "/", search: "", hash: "" };
	readonly pushCalls: string[] = [];
	readonly replaceCalls: string[] = [];
	private readonly listeners = new Set<EventListener>();
	private entries: string[];
	private index = 0;
	state: unknown = null;

	readonly dependencies: WorkspaceRouteHistoryDependencies = {
		history: this,
		location: this.location,
		events: this,
	};

	constructor(initialUrl: string) {
		this.entries = [initialUrl];
		this.sync(initialUrl);
	}

	get currentUrl() {
		return this.entries[this.index];
	}

	pushState(data: unknown, _unused: string, url?: string | URL | null): void {
		const next = String(url ?? this.currentUrl);
		this.entries = this.entries.slice(0, this.index + 1);
		this.entries.push(next);
		this.index += 1;
		this.state = data;
		this.pushCalls.push(next);
		this.sync(next);
	}

	replaceState(data: unknown, _unused: string, url?: string | URL | null): void {
		const next = String(url ?? this.currentUrl);
		this.entries[this.index] = next;
		this.state = data;
		this.replaceCalls.push(next);
		this.sync(next);
	}

	back(): void {
		if (this.index === 0) return;
		this.index -= 1;
		this.sync(this.currentUrl);
		this.emit();
	}

	forward(): void {
		if (this.index >= this.entries.length - 1) return;
		this.index += 1;
		this.sync(this.currentUrl);
		this.emit();
	}

	navigate(url: string): void {
		this.pushState(null, "", url);
		this.emit();
	}

	popstate(url: string): void {
		this.entries[this.index] = url;
		this.sync(url);
		this.emit();
	}

	addEventListener(_type: "popstate", listener: EventListener): void {
		this.listeners.add(listener);
	}

	removeEventListener(_type: "popstate", listener: EventListener): void {
		this.listeners.delete(listener);
	}

	private emit() {
		const event = { type: "popstate" } as Event;
		for (const listener of this.listeners) listener(event);
	}

	private sync(url: string) {
		const parsed = new URL(url, "https://example.test");
		this.location.pathname = parsed.pathname;
		this.location.search = parsed.search;
		this.location.hash = parsed.hash;
	}
}
