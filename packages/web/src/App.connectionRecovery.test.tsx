// @vitest-environment jsdom

import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { act, cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeAll, describe, expect, it, vi } from "vitest";
import type { AgentApi } from "./agentApi.ts";
import type { ConnectionStatus } from "./rpc.ts";
import type {
	Delegation,
	EventFrame,
	Project,
	SessionSnapshot,
	SessionSummary,
	TranscriptEntry,
	TranscriptTreeNode,
	TranscriptTurnsResult,
} from "./types.ts";
import { UI_RESUME_STORAGE_KEY } from "./uiResume.ts";

const mockedApi = vi.hoisted(() => ({ current: null as AgentApi | null }));

vi.mock("./agentApi.ts", async (importOriginal) => {
	const actual = await importOriginal<typeof import("./agentApi.ts")>();
	return {
		...actual,
		createAgentApi: () => {
			if (!mockedApi.current) throw new Error("App test API was not installed");
			return mockedApi.current;
		},
	};
});

import { App } from "./App.tsx";

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
	window.history.replaceState(null, "", "/");
	window.localStorage.clear();
	mockedApi.current = null;
});

describe("App connection recovery integration", () => {
	it("owns an initial project failure through deduplicated Retry, offline state, and reconnect recovery", async () => {
		const api = createControllableApi();
		const retry = deferred<Project[]>();
		api.listProjects
			.mockRejectedValueOnce(new Error("project list unavailable"))
			.mockImplementationOnce(() => retry.promise)
			.mockResolvedValue([project()]);
		const { client, unmount } = renderApp(api);

		await emitStatus(api, "open");
		let alert = await screen.findByRole("alert");
		expect(alert.textContent).toContain("Couldn’t load projects");
		expect(alert.textContent).toContain("project list unavailable");
		expect(document.body.textContent).not.toContain("Dismiss notification");

		const callsBeforeRetry = api.listProjects.mock.calls.length;
		const retryButton = within(alert).getByRole("button", { name: "Retry" });
		fireEvent.click(retryButton);
		fireEvent.click(retryButton);
		await waitFor(() => {
			expect(api.listProjects).toHaveBeenCalledTimes(callsBeforeRetry + 1);
			expect((within(alert).getByRole("button", { name: "Retrying…" }) as HTMLButtonElement).disabled).toBe(true);
		});

		await act(async () => retry.reject(new Error("project retry failed")));
		alert = await screen.findByRole("alert");
		expect(alert.textContent).toContain("project retry failed");
		await emitStatus(api, "closed");
		const offlineRetry = within(alert).getByRole("button", { name: "Retry" }) as HTMLButtonElement;
		expect(offlineRetry.disabled).toBe(true);
		expect(alert.textContent).toContain("Waiting for connection");

		await emitStatus(api, "open");
		expect(await screen.findByText("Recovered project")).toBeTruthy();
		await waitFor(() =>
			expect(screen.queryByText("project retry failed")).toBeNull(),
		);

		unmount();
		await client.cancelQueries();
		client.clear();
	});

	it("routes a cached project refresh failure through the disconnected banner and reconnect recovery", async () => {
		const api = createControllableApi();
		api.listProjects.mockResolvedValue([project()]);
		const { client, unmount } = renderApp(api);
		const recovery = deferred<Project[]>();

		await emitStatus(api, "open");
		expect(await screen.findByText("Recovered project")).toBeTruthy();
		api.listProjects
			.mockImplementationOnce(async () => {
				throw new Error("project refresh failed");
			})
			.mockImplementationOnce(() => recovery.promise);
		await act(async () => {
			await client.invalidateQueries({ queryKey: ["projects"] });
		});

		const banner = await screen.findByRole("status", { name: "Disconnected" });
		expect(banner.closest(".chat-dock")).toBeTruthy();
		expect(document.body.textContent).not.toContain("Project refresh failed");
		expect(document.body.textContent).not.toContain("project refresh failed");
		expect(document.querySelector(".project-load-error")).toBeNull();
		expect(screen.getByText("Recovered project")).toBeTruthy();

		const callsBeforeRetry = api.listProjects.mock.calls.length;
		const retry = within(banner).getByRole("button", { name: "Retry connection" });
		fireEvent.click(retry);
		fireEvent.click(retry);
		expect(api.reconnect).toHaveBeenCalledTimes(1);
		await emitStatus(api, "open");
		await waitFor(() =>
			expect(api.listProjects).toHaveBeenCalledTimes(callsBeforeRetry + 1));
		expect(screen.getByText("Recovered project")).toBeTruthy();
		expect(screen.getByText("Disconnected")).toBeTruthy();

		await act(async () => recovery.resolve([project({ name: "Restored project" })]));
		expect(await screen.findByText("Restored project")).toBeTruthy();
		await waitFor(() =>
			expect(screen.queryByRole("status", { name: "Disconnected" })).toBeNull());

		unmount();
		await client.cancelQueries();
		client.clear();
	});

	it("preserves the mounted draft/dialog, gates mutations, deduplicates Retry, and reconciles a later open", async () => {
		const api = createControllableApi();
		const retry = deferred<void>();
		api.setReconnectResult(retry.promise);
		const { client, unmount } = renderApp(api);
		const user = userEvent.setup();

		expect(screen.queryByText("Connecting…")).toBeNull();
		expect(api.connect).toHaveBeenCalledTimes(1);
		expect(api.listProjects).not.toHaveBeenCalled();
		expect(api.listSessions).not.toHaveBeenCalled();

		await openAndLoad(api);
		expect(screen.getByText("cached question")).toBeTruthy();
		expect(screen.getByText("cached answer")).toBeTruthy();
		expect(api.listProjects).toHaveBeenCalled();
		expect(api.listSessions).toHaveBeenCalled();
		expect(api.getSession).toHaveBeenCalledWith(SESSION_ID, { includeEntries: false });
		expect(api.getTranscriptTurns).toHaveBeenCalledWith(SESSION_ID, { limit: 50 });

		const composer = screen.getByRole("textbox", {
			name: "Enter for newline. Cmd+Enter to send.",
		}) as HTMLTextAreaElement;
		await user.type(composer, "keep this session draft");
		const send = screen.getByRole("button", { name: "send message" }) as HTMLButtonElement;
		const cachedNavigation = sessionNavigationButton();

		await user.click(screen.getByRole("button", { name: "new project" }));
		const projectInput = await screen.findByRole<HTMLInputElement>("textbox", { name: "Project name" });
		await user.type(projectInput, "Offline project draft");

		await emitStatus(api, "closed");
		await waitFor(() => expect(document.querySelector(".connection-recovery-banner")?.textContent).toContain("Disconnected"));
		await emitStatus(api, "connecting");
		expect(screen.getByText("Disconnected")).toBeTruthy();
		const save = screen.getByRole("button", { name: "Save" }) as HTMLButtonElement;
		expect(composer.value).toBe("keep this session draft");
		expect(projectInput.value).toBe("Offline project draft");
		expect(document.activeElement).toBe(projectInput);
		expect(save.disabled).toBe(true);
		expect(send.disabled).toBe(true);
		expect(cachedNavigation.disabled).toBe(false);

		await act(async () => {
			projectInput.closest("form")!.dispatchEvent(new Event("submit", { bubbles: true, cancelable: true }));
			send.click();
		});
		expect(api.createProject).not.toHaveBeenCalled();
		expect(api.queueFollowUp).not.toHaveBeenCalled();
		expect(api.startSession).not.toHaveBeenCalled();
		expect(totalMutationCalls(api)).toBe(0);

		const initialProjectLists = api.listProjects.mock.calls.length;
		const initialSessionLists = api.listSessions.mock.calls.length;
		const initialSelectedSyncs = api.getSession.mock.calls.length;
		const retryButton = document.querySelector<HTMLButtonElement>(".connection-retry-button");
		expect(retryButton?.textContent).toContain("Retry connection");
		retryButton!.click();
		retryButton!.click();

		expect(api.reconnect).toHaveBeenCalledTimes(1);
		await waitFor(() => {
			const pending = document.querySelector<HTMLButtonElement>(".connection-retry-button");
			expect(pending?.disabled).toBe(true);
			expect(pending?.getAttribute("aria-busy")).toBe("true");
			expect(pending?.textContent).toContain("Retrying…");
		});

		await emitStatus(api, "open");
		await waitFor(() => {
			expect(document.querySelector(".connection-recovery-banner")).toBeNull();
			expect(save.disabled).toBe(false);
			expect(send.disabled).toBe(false);
		});
		expect(screen.getByRole("textbox", { name: "Project name" })).toBe(projectInput);
		expect(projectInput.value).toBe("Offline project draft");
		expect(composer.value).toBe("keep this session draft");
		expect(document.activeElement).toBe(projectInput);
		await waitFor(() => {
			expect(api.listProjects.mock.calls.length).toBeGreaterThan(initialProjectLists);
			expect(api.listSessions.mock.calls.length).toBeGreaterThan(initialSessionLists);
			expect(api.getSession.mock.calls.length).toBeGreaterThan(initialSelectedSyncs);
		});

		await act(async () => {
			retry.reject(new Error("stale retry failure"));
			await retry.promise.catch(() => undefined);
		});
		expect(document.querySelector(".connection-recovery-banner")).toBeNull();
		expect(document.body.textContent).not.toContain("connection retry failed");
		expect(save.disabled).toBe(false);
		expect(document.activeElement).toBe(projectInput);

		unmount();
		await client.cancelQueries();
		client.clear();
		expect(api.statusListenerCount()).toBe(0);
		expect(api.eventListenerCount()).toBe(0);
		expect(api.close).toHaveBeenCalledTimes(1);
	});

	it("keeps cached transcript controls local and never reconnects through remote reads", async () => {
		const api = createControllableApi();
		const { client, unmount } = renderApp(api);
		const user = userEvent.setup();
		await openAndLoad(api);

		const cachedTurn = turnCardContaining("older cached question");
		await user.click(within(cachedTurn).getByRole("button", { name: "Show details" }));
		expect(await within(cachedTurn).findByText("cached detail evidence")).toBeTruthy();
		expect(api.getTranscriptTurnDetail).toHaveBeenCalledTimes(1);

		api.getSession.mockRejectedValueOnce(new Error("refresh failed"));
		await emitEvent(api, {
			event_id: 5,
			event: "session.recovered",
			session_id: SESSION_ID,
			data: { activity: "idle" },
		});
		expect(await screen.findByText("Session refresh failed")).toBeTruthy();

		await emitStatus(api, "closed");
		await waitFor(() => expect(screen.getByText("Disconnected")).toBeTruthy());

		const getSessionCalls = api.getSession.mock.calls.length;
		const getTurnsCalls = api.getTranscriptTurns.mock.calls.length;
		const getDetailCalls = api.getTranscriptTurnDetail.mock.calls.length;
		const retry = screen.getByRole("button", { name: "Retry" }) as HTMLButtonElement;
		const loadOlder = screen.getByRole("button", { name: "Load older turns" }) as HTMLButtonElement;
		const uncachedTurn = turnCardContaining("cached question");
		const uncachedShow = within(uncachedTurn).getByRole("button", { name: "Show details" }) as HTMLButtonElement;

		expect(retry.disabled).toBe(true);
		expect(loadOlder.disabled).toBe(true);
		expect(uncachedShow.disabled).toBe(true);
		expect(within(uncachedTurn).getByText("Waiting for connection").getAttribute("tabindex")).toBe("0");
		expect(loadOlder.parentElement?.textContent).toContain("Waiting for connection");
		expect(retry.parentElement?.textContent).toContain("Waiting for connection");

		retry.click();
		loadOlder.click();
		uncachedShow.click();
		expect(api.getSession).toHaveBeenCalledTimes(getSessionCalls);
		expect(api.getTranscriptTurns).toHaveBeenCalledTimes(getTurnsCalls);
		expect(api.getTranscriptTurnDetail).toHaveBeenCalledTimes(getDetailCalls);
		expect(api.reconnect).not.toHaveBeenCalled();

		const hideCached = within(cachedTurn).getByRole("button", { name: "Hide details" }) as HTMLButtonElement;
		expect(hideCached.disabled).toBe(false);
		await user.click(hideCached);
		expect(within(cachedTurn).queryByText("cached detail evidence")).toBeNull();
		const reopenCached = within(cachedTurn).getByRole("button", { name: "Show details" }) as HTMLButtonElement;
		expect(reopenCached.disabled).toBe(false);
		expect(reopenCached.parentElement?.textContent).not.toContain("Waiting for connection");
		await user.click(reopenCached);
		expect(await within(cachedTurn).findByText("cached detail evidence")).toBeTruthy();
		expect(api.getTranscriptTurnDetail).toHaveBeenCalledTimes(getDetailCalls);

		await emitStatus(api, "open");
		await waitFor(() => expect(api.getSession).toHaveBeenCalledTimes(getSessionCalls + 1));
		await waitFor(() => {
			expect((within(uncachedTurn).getByRole("button", { name: "Show details" }) as HTMLButtonElement).disabled).toBe(false);
			expect((screen.getByRole("button", { name: "Load older turns" }) as HTMLButtonElement).disabled).toBe(false);
		});
		expect(api.getTranscriptTurns).toHaveBeenCalledTimes(getTurnsCalls);

		await user.click(within(uncachedTurn).getByRole("button", { name: "Show details" }));
		expect(await within(uncachedTurn).findByText("uncached detail evidence")).toBeTruthy();
		expect(api.getTranscriptTurnDetail).toHaveBeenCalledTimes(getDetailCalls + 1);

		await user.click(screen.getByRole("button", { name: "Load older turns" }));
		await waitFor(() => expect(api.getTranscriptTurns).toHaveBeenCalledTimes(getTurnsCalls + 1));
		expect(api.reconnect).not.toHaveBeenCalled();

		unmount();
		await client.cancelQueries();
		client.clear();
		expect(api.statusListenerCount()).toBe(0);
		expect(api.eventListenerCount()).toBe(0);
	});

	it("applies replayed events before live events that arrive during replay", async () => {
		const api = createControllableApi();
		const replay = deferred<EventFrame[]>();
		api.subscribeEvents.mockImplementation(() => replay.promise);
		const { client, unmount } = renderApp(api);
		await openAndLoad(api);
		await waitFor(() => expect(api.subscribeEvents).toHaveBeenCalledWith(SESSION_ID, 4));

		await emitEvent(api, {
			event_id: 7,
			event: "turn.started",
			session_id: SESSION_ID,
			data: {},
		});
		expect(screen.queryByText("model error: replayed failure")).toBeNull();

		await act(async () => {
			replay.resolve([{
				event_id: 5,
				event: "model.error",
				session_id: SESSION_ID,
				data: { error: "replayed failure" },
			}]);
			await replay.promise;
		});
		expect(await screen.findByText("model error: replayed failure")).toBeTruthy();

		unmount();
		await client.cancelQueries();
		client.clear();
	});

	it("fences stale replay completion and failure across reconnect generations", async () => {
		const api = createControllableApi();
		const firstReplay = deferred<EventFrame[]>();
		const secondReplay = deferred<EventFrame[]>();
		const thirdReplay = deferred<EventFrame[]>();
		const replays = [firstReplay, secondReplay, thirdReplay];
		api.subscribeEvents.mockImplementation(() => replays.shift()!.promise);
		const { client, unmount } = renderApp(api);
		await openAndLoad(api);
		await waitFor(() => expect(api.subscribeEvents).toHaveBeenCalledTimes(1));

		await emitStatus(api, "closed");
		await emitStatus(api, "open");
		await waitFor(() => expect(api.subscribeEvents).toHaveBeenCalledTimes(2));
		await act(async () => {
			firstReplay.resolve([{
				event_id: 5,
				event: "model.error",
				session_id: SESSION_ID,
				data: { error: "stale replay completion" },
			}]);
			await firstReplay.promise;
		});
		expect(screen.queryByText("model error: stale replay completion")).toBeNull();

		await emitStatus(api, "closed");
		await emitStatus(api, "open");
		await waitFor(() => expect(api.subscribeEvents).toHaveBeenCalledTimes(3));
		await act(async () => {
			secondReplay.reject(new Error("stale replay failure"));
			await secondReplay.promise.catch(() => undefined);
		});
		expect(screen.queryByText("stale replay failure")).toBeNull();

		await emitEvent(api, {
			event_id: 9,
			event: "turn.started",
			session_id: SESSION_ID,
			data: {},
		});
		await act(async () => {
			thirdReplay.resolve([{
				event_id: 8,
				event: "model.error",
				session_id: SESSION_ID,
				data: { error: "current replay" },
			}]);
			await thirdReplay.promise;
		});
		expect(await screen.findByText("model error: current replay")).toBeTruthy();

		unmount();
		await client.cancelQueries();
		client.clear();
	});

	it("blocks initial-load Retry on connection error and loads canonically after open", async () => {
		const api = createControllableApi();
		api.getSession.mockRejectedValueOnce(new Error("initial load failed"));
		const { client, unmount } = renderApp(api);

		await emitStatus(api, "open");
		expect(await screen.findByText("Couldn’t load session")).toBeTruthy();
		await emitStatus(api, "error");

		const sessionCalls = api.getSession.mock.calls.length;
		const turnsCalls = api.getTranscriptTurns.mock.calls.length;
		const retry = screen.getByRole("button", { name: "Retry" }) as HTMLButtonElement;
		expect(retry.disabled).toBe(true);
		expect(retry.parentElement?.textContent).toContain("Waiting for connection");
		retry.click();
		expect(api.getSession).toHaveBeenCalledTimes(sessionCalls);
		expect(api.getTranscriptTurns).toHaveBeenCalledTimes(turnsCalls);
		expect(api.reconnect).not.toHaveBeenCalled();

		await emitStatus(api, "open");
		await waitFor(() => expect(api.getSession).toHaveBeenCalledTimes(sessionCalls + 3));
		expect(api.getTranscriptTurns).toHaveBeenCalledTimes(turnsCalls + 1);
		expect(await screen.findByText("cached answer")).toBeTruthy();

		unmount();
		await client.cancelQueries();
		client.clear();
		expect(api.statusListenerCount()).toBe(0);
		expect(api.eventListenerCount()).toBe(0);
	});

	it("targets delegation cancellation to the rendered parent and keeps terminal work inert", async () => {
		const api = createControllableApi();
		const running = appDelegation({
			delegation_id: "cancel-target",
			label: "Cancel target",
			status: "running",
			progress: { expected: 1, spawned: 1, terminal: 0, running: 1, failed: 0 },
			subagents: [{
				id: "running-child",
				status: "running",
				activity: "running",
				role: "implementer",
				subagent_type: "full",
				task_prompt_file: "running-child/task_prompt.md",
			}],
		});
		const finished = appDelegation({
			delegation_id: "finished-target",
			label: "Finished target",
			status: "failed",
			progress: { expected: 1, spawned: 1, terminal: 1, running: 0, failed: 1 },
			subagents: [{
				id: "finished-child",
				status: "failed",
				activity: "idle",
				role: "implementer",
				subagent_type: "full",
				task_prompt_file: "finished-child/task_prompt.md",
			}],
		});
		api.listDelegations.mockResolvedValue({
			parent_session_id: SESSION_ID,
			has_more: false,
			delegations: [running, finished],
		});
		api.getSession.mockImplementation(async (sessionId: string) => {
			if (sessionId === SESSION_ID) return sessionSnapshot();
			return {
				...sessionSnapshot(),
				session_id: sessionId,
				parent_session_id: SESSION_ID,
				delegation_id: sessionId === "running-child" ? "cancel-target" : "finished-target",
				activity: sessionId === "running-child" ? "running" : "idle",
				active_leaf_id: null,
				has_transcript_entries: false,
			};
		});
		api.cancelDelegation.mockResolvedValue({ cancelled: true });
		const { client, unmount } = renderApp(api);
		const user = userEvent.setup();
		await openAndLoad(api);

		const cancelTarget = screen.getByRole("article", { name: /Cancel target/ });
		await emitStatus(api, "closed");
		const blockedCancel = within(cancelTarget).getByRole("button", { name: "stop delegated work" }) as HTMLButtonElement;
		expect(blockedCancel.disabled).toBe(true);
		expect(cancelTarget.textContent).toContain("Waiting for connection");
		fireEvent.click(blockedCancel);
		expect(api.cancelDelegation).not.toHaveBeenCalled();

		await emitStatus(api, "open");
		const enabledCancel = within(cancelTarget).getByRole("button", { name: "stop delegated work" }) as HTMLButtonElement;
		await waitFor(() => expect(enabledCancel.disabled).toBe(false));
		await user.click(enabledCancel);
		await waitFor(() => {
			expect(api.cancelDelegation).toHaveBeenCalledWith(SESSION_ID, "cancel-target");
		});

		const finishedTarget = screen.getByRole("article", { name: /Finished target/ });
		expect(within(finishedTarget).queryByRole("button", { name: "stop delegated work" })).toBeNull();
		expect(api.readHandoffFile).not.toHaveBeenCalled();
		expect(api.startFullDelegation).not.toHaveBeenCalled();

		const childRow = screen.getByRole("button", {
			name: /Open agent Cached session, implementer, running/,
		});
		await user.click(childRow);
		await waitFor(() => expect(childRow.getAttribute("aria-current")).toBe("page"));
		expect(api.listDelegations.mock.calls.some(([parent]) => parent === SESSION_ID)).toBe(true);

		unmount();
		await client.cancelQueries();
		client.clear();
	});

	it("names a subagent from delegation.list title even when the child warm fails", async () => {
		const api = createControllableApi();
		api.listDelegations.mockResolvedValue({
			parent_session_id: SESSION_ID,
			has_more: false,
			delegations: [
				appDelegation({
					delegation_id: "titled",
					label: "Titled work",
					status: "running",
					progress: { expected: 1, spawned: 1, terminal: 0, running: 1, failed: 0 },
					subagents: [{
						id: "titled-child",
						status: "running",
						activity: "running",
						role: "implementer",
						title: "Wire up the store",
						subagent_type: "full",
						task_prompt_file: "titled-child/task_prompt.md",
					}],
				}),
			],
		});
		// The child session never yields a usable snapshot: its warm read fails, so
		// the only possible name source is `delegation.list`'s `title`.
		api.getSession.mockImplementation(async (sessionId: string) => {
			if (sessionId === SESSION_ID) return sessionSnapshot();
			throw new Error(`no warm for ${sessionId}`);
		});
		const { client, unmount } = renderApp(api);
		await openAndLoad(api);

		expect(
			await screen.findByRole("button", {
				name: /Open agent Wire up the store, implementer, running/,
			}),
		).toBeTruthy();

		unmount();
		await client.cancelQueries();
		client.clear();
	});

	it("keeps effort editable during a running response while model stays idle-gated", async () => {
		const api = createControllableApi();
		const running = {
			...sessionSnapshot(),
			activity: "running" as const,
			provider: { kind: "openai" as const, model: "gpt-5.1", reasoning_effort: "medium" as const },
		};
		api.getSession.mockResolvedValue(running);
		api.listSessions.mockResolvedValue([{ ...sessionSummary(), ...running }]);
		api.configureSession.mockResolvedValue({
			session_id: SESSION_ID,
			activity: "running",
			provider: { ...running.provider, reasoning_effort: "high" },
			metadata: running.metadata,
		});
		const { client, unmount } = renderApp(api);
		await openAndLoad(api);

		const model = screen.getByRole("combobox", { name: "Model" }) as HTMLSelectElement;
		const effort = screen.getByRole("combobox", { name: "Reasoning effort" }) as HTMLSelectElement;
		expect(model.disabled).toBe(true);
		expect(effort.disabled).toBe(false);
		expect(document.body.textContent).not.toContain("Applies next turn");

		await emitStatus(api, "closed");
		await waitFor(() => expect(effort.disabled).toBe(true));
		await emitStatus(api, "open");
		await waitFor(() => expect(effort.disabled).toBe(false));

		fireEvent.change(effort, { target: { value: "high" } });
		await waitFor(() =>
			expect(api.configureSession).toHaveBeenCalledWith({
				sessionId: SESSION_ID,
				provider: { ...running.provider, reasoning_effort: "high" },
			}),
		);
		expect(effort.value).toBe("high");

		unmount();
		await client.cancelQueries();
		client.clear();
	});

	it("routes a cached agent refresh failure through the disconnected banner and reconnect recovery", async () => {
		const api = createControllableApi();
		const firstExpansion = deferred<ReturnType<typeof delegationPage>>();
		const retryExpansion = deferred<ReturnType<typeof delegationPage>>();
		const defaultPage = delegationPage(
			["Recent 1", "Recent 2", "Recent 3"],
			{ hasMore: true, limit: 3 },
		);
		const expandedPage = delegationPage(
			Array.from({ length: 100 }, (_, index) => `Expanded ${index + 1}`),
			{ hasMore: true, limit: 100 },
		);
		let expansionCalls = 0;
		api.listDelegations.mockImplementation(async (parentSessionId: string, limit?: number) => {
			if (parentSessionId !== SESSION_ID) throw new Error("unexpected parent");
			if (limit === 3) return defaultPage;
			if (limit === 100) {
				expansionCalls += 1;
				return expansionCalls === 1 ? firstExpansion.promise : retryExpansion.promise;
			}
			throw new Error(`unexpected delegation limit ${String(limit)}`);
		});
		const { client, unmount } = renderApp(api);
		const user = userEvent.setup();
		await openAndLoad(api);
		expect(await screen.findByRole("article", { name: /Recent 1/ })).toBeTruthy();

		await user.click(screen.getByRole("button", { name: /see more/i }));
		expect(screen.getByRole("article", { name: /Recent 1/ })).toBeTruthy();
		expect(screen.getByRole("button", { name: /show fewer/i })).toBeTruthy();
		expect(api.listDelegations).toHaveBeenCalledWith(SESSION_ID, 100);

		firstExpansion.reject(new Error("100-row load failed"));
		const banner = await screen.findByRole("status", { name: "Disconnected" });
		expect(banner.closest(".chat-dock")).toBeTruthy();
		expect(document.body.textContent).not.toContain("Agent refresh failed");
		expect(document.body.textContent).not.toContain("100-row load failed");
		expect(document.querySelector(".run-board-load-error")).toBeNull();
		expect(screen.getByRole("article", { name: /Recent 1/ })).toBeTruthy();
		expect(screen.getByRole("button", { name: /show fewer/i })).toBeTruthy();

		const expandedCallsBeforeRetry = api.listDelegations.mock.calls.filter(
			([, limit]) => limit === 100,
		).length;
		const retry = within(banner).getByRole("button", { name: "Retry connection" });
		fireEvent.click(retry);
		fireEvent.click(retry);
		expect(api.reconnect).toHaveBeenCalledTimes(1);
		await emitStatus(api, "open");
		await waitFor(() =>
			expect(api.listDelegations.mock.calls.filter(([, limit]) => limit === 100))
				.toHaveLength(expandedCallsBeforeRetry + 1));
		expect(screen.getByText("Disconnected")).toBeTruthy();
		expect(screen.getByRole("article", { name: /Recent 1/ })).toBeTruthy();

		retryExpansion.resolve(expandedPage);
		expect(await screen.findByRole("article", { name: /Expanded 100/ })).toBeTruthy();
		await waitFor(() =>
			expect(screen.queryByRole("status", { name: "Disconnected" })).toBeNull());
		expect(screen.queryByRole("article", { name: /Recent 1/ })).toBeNull();
		expect(screen.getByText("Latest 100 shown.")).toBeTruthy();

		await user.click(screen.getByRole("button", { name: /show fewer/i }));
		expect(screen.getByRole("article", { name: /Recent 1/ })).toBeTruthy();
		expect(screen.queryByRole("article", { name: /Expanded 100/ })).toBeNull();
		const callsBeforeOfflineReopen = api.listDelegations.mock.calls.length;
		await emitStatus(api, "closed");
		const cachedSeeMore = screen.getByRole("button", { name: /see more/i }) as HTMLButtonElement;
		expect(cachedSeeMore.disabled).toBe(false);
		await user.click(cachedSeeMore);
		expect(screen.getByRole("article", { name: /Expanded 100/ })).toBeTruthy();
		expect(api.listDelegations).toHaveBeenCalledTimes(callsBeforeOfflineReopen);

		unmount();
		await client.cancelQueries();
		client.clear();
	});

	it("fences a pending 100-row result when the selected parent changes", async () => {
		const api = createControllableApi();
		const staleExpansion = deferred<ReturnType<typeof delegationPage>>();
		const secondSessionId = "session-2";
		api.listSessions.mockResolvedValue([
			sessionSummary(),
			{
				...sessionSummary(),
				session_id: secondSessionId,
				metadata: { title: "Second parent" },
			},
		]);
		api.getSession.mockImplementation(async (sessionId: string) => ({
			...sessionSnapshot(),
			session_id: sessionId,
			metadata: {
				title: sessionId === secondSessionId ? "Second parent" : SESSION_TITLE,
			},
		}));
		api.listDelegations.mockImplementation(async (parentSessionId: string, limit?: number) => {
			if (parentSessionId === SESSION_ID && limit === 3) {
				return delegationPage(["First parent row"], { hasMore: true, limit: 3 });
			}
			if (parentSessionId === SESSION_ID && limit === 100) return staleExpansion.promise;
			if (parentSessionId === secondSessionId && limit === 3) {
				return {
					...delegationPage(["Second parent row"], { hasMore: false, limit: 3 }),
					parent_session_id: secondSessionId,
				};
			}
			throw new Error(`unexpected delegation request ${parentSessionId}:${String(limit)}`);
		});
		const { client, unmount } = renderApp(api);
		const user = userEvent.setup();
		await openAndLoad(api);
		expect(await screen.findByRole("article", { name: /First parent row/ })).toBeTruthy();

		await user.click(screen.getByRole("button", { name: /see more/i }));
		const secondParentButtons = screen.getAllByRole("button", { name: /Second parent/ });
		const secondParentNavigation = secondParentButtons.find(
			(button) => !button.hasAttribute("aria-haspopup"),
		);
		if (!secondParentNavigation) throw new Error("missing second parent navigation");
		await user.click(secondParentNavigation);

		expect(await screen.findByRole("article", { name: /Second parent row/ })).toBeTruthy();
		expect(screen.queryByRole("article", { name: /First parent row/ })).toBeNull();
		staleExpansion.resolve(
			delegationPage(["Stale expanded parent row"], { hasMore: false, limit: 100 }),
		);
		await act(async () => {
			await staleExpansion.promise;
			await Promise.resolve();
		});
		expect(screen.queryByRole("article", { name: /Stale expanded parent row/ })).toBeNull();
		expect(screen.getByRole("article", { name: /Second parent row/ })).toBeTruthy();

		unmount();
		await client.cancelQueries();
		client.clear();
	});
});

const SESSION_ID = "session-1";
const SESSION_TITLE = "Cached session";

type ApiSpy = ReturnType<typeof vi.fn>;

type ControllableApi = AgentApi & {
	connect: ApiSpy;
	reconnect: ApiSpy;
	close: ApiSpy;
	listProjects: ApiSpy;
	listSessions: ApiSpy;
	listDelegations: ApiSpy;
	getSession: ApiSpy;
	getTranscriptTurns: ApiSpy;
	getTranscriptTurnDetail: ApiSpy;
	subscribeEvents: ApiSpy;
	startSession: ApiSpy;
	queueFollowUp: ApiSpy;
	renameSession: ApiSpy;
	createProject: ApiSpy;
	updateProject: ApiSpy;
	deleteProject: ApiSpy;
	configureSession: ApiSpy;
	deleteSession: ApiSpy;
	interrupt: ApiSpy;
	resumeTurn: ApiSpy;
	switchHistory: ApiSpy;
	promoteQueuedInput: ApiSpy;
	updateQueuedInput: ApiSpy;
	cancelQueuedInput: ApiSpy;
	reorderQueuedFollowUps: ApiSpy;
	requestCompaction: ApiSpy;
	readHandoffFile: ApiSpy;
	startFullDelegation: ApiSpy;
	startReadonlyDelegationFanout: ApiSpy;
	cancelDelegation: ApiSpy;
	steerSubagent: ApiSpy;
	emitStatus(status: ConnectionStatus): void;
	emitEvent(event: EventFrame): void;
	setReconnectResult(result: Promise<void>): void;
	statusListenerCount(): number;
	eventListenerCount(): number;
};

function renderApp(api: ControllableApi) {
	window.localStorage.setItem(
		UI_RESUME_STORAGE_KEY,
		JSON.stringify({
			selectedProjectId: null,
			selectedSessionIdByProject: { __host__: SESSION_ID },
			updatedAt: 1,
		}),
	);
	mockedApi.current = api;
	const client = new QueryClient({
		defaultOptions: {
			queries: {
				retry: false,
				gcTime: Infinity,
				refetchOnWindowFocus: false,
			},
			mutations: { retry: false },
		},
	});
	const result = render(
		<QueryClientProvider client={client}>
			<App />
		</QueryClientProvider>,
	);
	return { ...result, client };
}

async function openAndLoad(api: ControllableApi) {
	await emitStatus(api, "open");
	await waitFor(() => {
		expect(screen.queryByText("Connecting…")).toBeNull();
		expect(screen.getByText("cached answer")).toBeTruthy();
	});
}

async function emitEvent(api: ControllableApi, event: EventFrame) {
	await act(async () => {
		api.emitEvent(event);
		await new Promise((resolve) => window.setTimeout(resolve, 100));
	});
}

function turnCardContaining(text: string): HTMLElement {
	const card = screen.getByText(text).closest(".turn-summary");
	if (!(card instanceof HTMLElement)) throw new Error(`missing turn card containing ${text}`);
	return card;
}

async function emitStatus(api: ControllableApi, status: ConnectionStatus) {
	await act(async () => {
		api.emitStatus(status);
		await new Promise((resolve) => window.setTimeout(resolve, 0));
	});
}

function sessionNavigationButton(): HTMLButtonElement {
	return sessionNavigationButtonNamed(SESSION_TITLE);
}

function sessionNavigationButtonNamed(title: string): HTMLButtonElement {
	const candidates = screen.getAllByRole("button", { name: new RegExp(title) });
	const navigation = candidates.find((button) => !button.hasAttribute("aria-haspopup"));
	if (!(navigation instanceof HTMLButtonElement)) throw new Error("missing cached session navigation");
	return navigation;
}

function totalMutationCalls(api: ControllableApi): number {
	return [
		api.startSession,
		api.queueFollowUp,
		api.renameSession,
		api.createProject,
		api.updateProject,
		api.deleteProject,
		api.configureSession,
		api.deleteSession,
		api.interrupt,
		api.resumeTurn,
		api.switchHistory,
		api.promoteQueuedInput,
		api.updateQueuedInput,
		api.cancelQueuedInput,
		api.reorderQueuedFollowUps,
		api.requestCompaction,
		api.startFullDelegation,
		api.startReadonlyDelegationFanout,
		api.cancelDelegation,
		api.steerSubagent,
	].reduce((total, spy) => total + spy.mock.calls.length, 0);
}

function createControllableApi(): ControllableApi {
	let status: ConnectionStatus = "connecting";
	let reconnectResult = Promise.resolve();
	const statusListeners = new Set<(next: ConnectionStatus) => void>();
	const eventListeners = new Set<(event: EventFrame) => void>();
	const mutation = () => vi.fn(async () => {
		throw new Error("unexpected mutation");
	});
	const api = {
		connect: vi.fn(async () => undefined),
		reconnect: vi.fn(() => reconnectResult),
		close: vi.fn(),
		isOpen: vi.fn(() => status === "open"),
		onStatus: vi.fn((listener: (next: ConnectionStatus) => void) => {
			statusListeners.add(listener);
			return () => statusListeners.delete(listener);
		}),
		onEvent: vi.fn((listener: (event: EventFrame) => void) => {
			eventListeners.add(listener);
			return () => eventListeners.delete(listener);
		}),
		listProjects: vi.fn(async () => []),
		listSessions: vi.fn(async () => [sessionSummary()]),
		listDelegations: vi.fn(async () => ({
			parent_session_id: SESSION_ID,
			has_more: false,
			delegations: [],
		})),
		listTools: vi.fn(async () => []),
		getSession: vi.fn(async () => sessionSnapshot()),
		getTranscriptTurns: vi.fn(async (_sessionId: string, options: { beforeEntryId?: string } = {}) =>
			options.beforeEntryId ? olderTranscriptTurns(options.beforeEntryId) : transcriptTurns()),
		getTranscriptTurnDetail: vi.fn(async (_sessionId: string, options: { cardId: string }) => ({
			session_id: SESSION_ID,
			active_leaf_id: "entry-finish",
			session_revision: 2,
			transcript_revision: 2,
			card_id: options.cardId,
			entries: options.cardId === "entry-finish-1" ? firstTurnDetail() : secondTurnDetail(),
		})),
		getTranscriptIndex: vi.fn(async () => transcriptIndex()),
		getTranscriptEntries: vi.fn(async () => ({
			session_id: SESSION_ID,
			session_revision: 2,
			transcript_revision: 2,
			entries: [],
		})),
		getHistoryTree: vi.fn(async () => ({
			session_id: SESSION_ID,
			active_leaf_id: "entry-finish",
			entries: [],
		})),
		getHistoryContext: vi.fn(async () => []),
		getSystemPrompt: vi.fn(async () => ({ template: "", rendered: null })),
		syncActiveBranch: vi.fn(async () => ({
			session_id: SESSION_ID,
			base_leaf_id: "entry-finish",
			active_leaf_id: "entry-finish",
			status: "unchanged" as const,
			entries: [],
			overview: sessionSnapshot(),
		})),
		subscribeEvents: vi.fn(async () => []),
		unsubscribeEvents: vi.fn(async () => undefined),
		readHandoffFile: vi.fn(async () => ({
			delegation_id: "delegation-1",
			subagent_id: null,
			file: "task_prompt.md" as const,
			content: "",
		})),
		startSession: mutation(),
		queueFollowUp: mutation(),
		interrupt: mutation(),
		resumeTurn: mutation(),
		switchHistory: mutation(),
		renameSession: mutation(),
		deleteSession: mutation(),
		configureSession: mutation(),
		createProject: mutation(),
		updateProject: mutation(),
		deleteProject: mutation(),
		promoteQueuedInput: mutation(),
		updateQueuedInput: mutation(),
		cancelQueuedInput: mutation(),
		reorderQueuedFollowUps: mutation(),
		requestCompaction: mutation(),
		startFullDelegation: mutation(),
		startReadonlyDelegationFanout: mutation(),
		cancelDelegation: mutation(),
		steerSubagent: mutation(),
		emitStatus(next: ConnectionStatus) {
			status = next;
			for (const listener of statusListeners) listener(next);
		},
		emitEvent(event: EventFrame) {
			for (const listener of eventListeners) listener(event);
		},
		setReconnectResult(result: Promise<void>) {
			reconnectResult = result;
		},
		statusListenerCount: () => statusListeners.size,
		eventListenerCount: () => eventListeners.size,
	} as unknown as ControllableApi;
	return api;
}

function project(overrides: Partial<Project> = {}): Project {
	return {
		project_id: "project-recovered",
		name: "Recovered project",
		workspaces: [],
		metadata: {},
		created_at: "2026-01-01T00:00:00Z",
		updated_at: "2026-01-01T00:00:00Z",
		...overrides,
	};
}

function sessionSummary(): SessionSummary {
	return {
		session_id: SESSION_ID,
		project_id: null,
		runtime_id: "runtime-test",
	workspace_id: "workspace-test",
		workspaces: [],
		activity: "idle",
		active_leaf_id: "entry-finish",
		provider: { kind: "openai", model: "gpt-5.1" },
		metadata: { title: SESSION_TITLE },
		created_at: "2026-01-01T00:00:00Z",
		updated_at: "2026-01-01T00:00:01Z",
		has_transcript_entries: true,
	};
}

function delegationPage(
	labels: string[],
	{ hasMore, limit }: { hasMore: boolean; limit: number },
) {
	return {
		parent_session_id: SESSION_ID,
		limit,
		has_more: hasMore,
		delegations: labels.map((label, index) =>
			appDelegation({
				delegation_id: `delegation-${limit}-${index + 1}`,
				label,
				subagents: [],
			})),
	};
}

function appDelegation(overrides: Partial<Delegation> = {}): Delegation {
	return {
		delegation_id: "delegation-1",
		kind: "full",
		status: "done",
		workflow: "workflow-implement-review",
		label: "Delegated work",
		progress: { expected: 1, spawned: 1, terminal: 1, running: 0, failed: 0 },
		subagents: [],
		...overrides,
	};
}

function sessionSnapshot(): SessionSnapshot {
	return {
		...sessionSummary(),
		pending_actions: [],
		queued_inputs: [],
		session_revision: 2,
		queue_revision: 1,
		transcript_revision: 2,
		last_event_id: 4,
		server_time_ms: 1_700_000_000_004,
	};
}

function transcriptTurns(): TranscriptTurnsResult {
	return {
		session_id: SESSION_ID,
		active_leaf_id: "entry-finish",
		session_revision: 2,
		transcript_revision: 2,
		before_entry_id: null,
		next_before_entry_id: "entry-start-1",
		has_more_before: true,
		limit: 50,
		cards: [
			{
				id: "entry-finish-1",
				turn_id: 1,
				status: "completed",
				outcome: "Graceful",
				start_entry_id: "entry-start-1",
				boundary_entry_id: "entry-finish-1",
				active_leaf_id: "entry-finish-1",
				start_sequence: 1,
				end_sequence: 4,
				start_timestamp_ms: 1_700_000_000_001,
				timestamp_ms: 1_700_000_000_004,
				user_messages: [firstUserEntry()],
				assistant_message: firstAssistantEntry(),
				summary: null,
				can_resume: false,
			},
			{
				id: "entry-finish",
				turn_id: 2,
				status: "completed",
				outcome: "Graceful",
				start_entry_id: "entry-start-2",
				boundary_entry_id: "entry-finish",
				active_leaf_id: "entry-finish",
				start_sequence: 5,
				end_sequence: 8,
				start_timestamp_ms: 1_700_000_000_005,
				timestamp_ms: 1_700_000_000_008,
				user_messages: [userEntry()],
				assistant_message: assistantEntry(),
				summary: null,
				can_resume: false,
			},
		],
	};
}

function olderTranscriptTurns(beforeEntryId: string): TranscriptTurnsResult {
	return {
		session_id: SESSION_ID,
		active_leaf_id: "entry-finish",
		session_revision: 2,
		transcript_revision: 2,
		before_entry_id: beforeEntryId,
		next_before_entry_id: null,
		has_more_before: false,
		limit: 50,
		cards: [],
	};
}

function firstTurnDetail(): TranscriptEntry[] {
	return [
		{
			id: "entry-start-1",
			parent_id: null,
			sequence: 1,
			timestamp_ms: 1_700_000_000_001,
			item: { type: "turn_started", turn_id: 1 },
		},
		firstUserEntry(),
		{
			id: "entry-progress-1",
			parent_id: "entry-user-1",
			sequence: 3,
			timestamp_ms: 1_700_000_000_003,
			item: {
				type: "assistant_message",
				items: [{ type: "text", text: "cached detail evidence" }],
			},
		},
		{
			id: "entry-finish-1",
			parent_id: "entry-progress-1",
			sequence: 4,
			timestamp_ms: 1_700_000_000_004,
			item: { type: "turn_finished", turn_id: 1, outcome: "Graceful" },
		},
	];
}

function secondTurnDetail(): TranscriptEntry[] {
	return [
		{
			id: "entry-start-2",
			parent_id: "entry-finish-1",
			sequence: 5,
			timestamp_ms: 1_700_000_000_005,
			item: { type: "turn_started", turn_id: 2 },
		},
		userEntry(),
		{
			id: "entry-progress-2",
			parent_id: "entry-user",
			sequence: 7,
			timestamp_ms: 1_700_000_000_007,
			item: {
				type: "assistant_message",
				items: [{ type: "text", text: "uncached detail evidence" }],
			},
		},
		{
			id: "entry-finish",
			parent_id: "entry-progress-2",
			sequence: 8,
			timestamp_ms: 1_700_000_000_008,
			item: { type: "turn_finished", turn_id: 2, outcome: "Graceful" },
		},
	];
}

function firstUserEntry(): TranscriptEntry {
	return {
		id: "entry-user-1",
		parent_id: "entry-start-1",
		sequence: 2,
		timestamp_ms: 1_700_000_000_002,
		item: {
			type: "user_message",
			content: [{ type: "text", text: "older cached question" }],
		},
	};
}

function firstAssistantEntry(): TranscriptEntry {
	return {
		id: "entry-assistant-1",
		parent_id: "entry-user-1",
		sequence: 3,
		timestamp_ms: 1_700_000_000_003,
		item: {
			type: "assistant_message",
			items: [{ type: "text", text: "older cached answer" }],
		},
	};
}

function userEntry(): TranscriptEntry {
	return {
		id: "entry-user",
		parent_id: "entry-start-2",
		sequence: 6,
		timestamp_ms: 1_700_000_000_006,
		item: {
			type: "user_message",
			content: [{ type: "text", text: "cached question" }],
		},
	};
}

function assistantEntry(): TranscriptEntry {
	return {
		id: "entry-assistant",
		parent_id: "entry-user",
		sequence: 7,
		timestamp_ms: 1_700_000_000_007,
		item: {
			type: "assistant_message",
			items: [{ type: "text", text: "cached answer" }],
		},
	};
}

function transcriptIndex() {
	const nodes: TranscriptTreeNode[] = [
		{
			id: "entry-finish",
			parent_id: "entry-assistant",
			timestamp_ms: 1_700_000_000_004,
			sequence: 4,
			item_type: "turn_finished",
			turn_id: 1,
			outcome: "Graceful",
			can_switch_to: true,
			edit_target_leaf_id: null,
			display_hint: "cached answer",
		},
	];
	return {
		session_id: SESSION_ID,
		active_leaf_id: "entry-finish",
		session_revision: 2,
		transcript_revision: 2,
		after_sequence: 0,
		max_sequence: 4,
		complete: true,
		nodes,
	};
}

function deferred<T>() {
	let resolve!: (value: T | PromiseLike<T>) => void;
	let reject!: (reason?: unknown) => void;
	const promise = new Promise<T>((resolvePromise, rejectPromise) => {
		resolve = resolvePromise;
		reject = rejectPromise;
	});
	return { promise, resolve, reject };
}
