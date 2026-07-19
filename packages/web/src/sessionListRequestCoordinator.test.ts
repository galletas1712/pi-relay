import { QueryClient, QueryObserver } from "@tanstack/react-query";
import { describe, expect, it, vi } from "vitest";
import { queryKeys } from "./queryKeys.ts";
import { SessionListRequestCoordinator } from "./sessionListRequestCoordinator.ts";
import type { SessionSummary } from "./types.ts";

interface Deferred<T> {
	promise: Promise<T>;
	resolve: (value: T) => void;
	reject: (reason: unknown) => void;
}

function deferred<T>(): Deferred<T> {
	let resolve!: (value: T) => void;
	let reject!: (reason: unknown) => void;
	const promise = new Promise<T>((resolvePromise, rejectPromise) => {
		resolve = resolvePromise;
		reject = rejectPromise;
	});
	return { promise, resolve, reject };
}

function session(sessionId: string): SessionSummary {
	return {
		session_id: sessionId,
		project_id: "project-a",
		runtime_id: "runtime-test",
	workspace_id: "workspace-test",
		workspaces: [],
		activity: "idle",
		active_leaf_id: null,
		provider: { kind: "openai", model: "gpt-test" },
		metadata: { title: sessionId },
		created_at: "2024-01-01T00:00:00Z",
		updated_at: "2024-01-01T00:00:00Z",
	};
}

async function settle(): Promise<void> {
	await Promise.resolve();
	await Promise.resolve();
	await Promise.resolve();
}

describe("session list request coordinator", () => {
	it("stays busy through TanStack's retry delay and only clears the error on terminal success", async () => {
		vi.useFakeTimers();
		const queryClient = new QueryClient({
			defaultOptions: { queries: { retry: 1, retryDelay: 1_000 } },
		});
		const coordinator = new SessionListRequestCoordinator<SessionSummary[]>("project-a");
		const attempts = [deferred<SessionSummary[]>(), deferred<SessionSummary[]>()];
		let attemptIndex = 0;
		const observer = new QueryObserver(queryClient, {
			queryKey: queryKeys.sessions("project-a"),
			queryFn: () =>
				coordinator.run("project-a", () => attempts[attemptIndex++].promise),
			enabled: false,
		});
		const unsubscribe = observer.subscribe((result) => {
			coordinator.setQueryFetching("project-a", result.fetchStatus === "fetching");
		});

		try {
			const fetch = observer.refetch();
			await settle();
			expect(attemptIndex).toBe(1);
			expect(coordinator.getSnapshot()).toMatchObject({ error: null, busy: true });

			attempts[0].reject(new Error("first attempt failed"));
			await settle();
			expect(attemptIndex).toBe(1);
			expect(coordinator.getSnapshot()).toMatchObject({
				error: "first attempt failed",
				busy: true,
			});

			await vi.advanceTimersByTimeAsync(999);
			expect(attemptIndex).toBe(1);
			expect(coordinator.getSnapshot()).toMatchObject({
				error: "first attempt failed",
				busy: true,
			});

			await vi.advanceTimersByTimeAsync(1);
			await settle();
			expect(attemptIndex).toBe(2);
			expect(coordinator.getSnapshot()).toMatchObject({
				error: "first attempt failed",
				busy: true,
			});

			attempts[1].resolve([session("restored")]);
			await fetch;
			await settle();
			expect(coordinator.getSnapshot()).toMatchObject({ error: null, busy: false });
		} finally {
			unsubscribe();
			queryClient.clear();
			vi.useRealTimers();
		}
	});

	it("stays busy through TanStack's retry delay and exposes only the terminal error afterward", async () => {
		vi.useFakeTimers();
		const queryClient = new QueryClient({
			defaultOptions: { queries: { retry: 1, retryDelay: 1_000 } },
		});
		const coordinator = new SessionListRequestCoordinator<SessionSummary[]>("project-a");
		const attempts = [deferred<SessionSummary[]>(), deferred<SessionSummary[]>()];
		let attemptIndex = 0;
		const observer = new QueryObserver(queryClient, {
			queryKey: queryKeys.sessions("project-a"),
			queryFn: () =>
				coordinator.run("project-a", () => attempts[attemptIndex++].promise),
			enabled: false,
		});
		const unsubscribe = observer.subscribe((result) => {
			coordinator.setQueryFetching("project-a", result.fetchStatus === "fetching");
		});

		try {
			const fetch = observer.refetch();
			await settle();
			attempts[0].reject(new Error("first attempt failed"));
			await settle();
			expect(coordinator.getSnapshot()).toMatchObject({
				error: "first attempt failed",
				busy: true,
			});

			await vi.advanceTimersByTimeAsync(1_000);
			await settle();
			attempts[1].reject(new Error("terminal failure"));
			await fetch;
			await settle();

			expect(coordinator.getSnapshot()).toMatchObject({
				error: "terminal failure",
				busy: false,
			});
		} finally {
			unsubscribe();
			queryClient.clear();
			vi.useRealTimers();
		}
	});

	it("adopts a destination project's pending canonical failure when that project is selected", async () => {
		const queryClient = new QueryClient({
			defaultOptions: { queries: { retry: false } },
		});
		const coordinator = new SessionListRequestCoordinator<SessionSummary[]>("project-a");
		const background = deferred<SessionSummary[]>();
		let requests = 0;
		const observer = new QueryObserver(queryClient, {
			queryKey: queryKeys.sessions("project-b"),
			queryFn: () =>
				coordinator.run("project-b", () => {
					requests += 1;
					return background.promise;
				}),
			enabled: false,
		});
		const unsubscribe = observer.subscribe((result) => {
			coordinator.setQueryFetching("project-b", result.fetchStatus === "fetching");
		});

		try {
			const fetch = observer.refetch();
			await settle();
			expect(requests).toBe(1);
			expect(coordinator.getSnapshot()).toMatchObject({
				projectId: "project-a",
				busy: false,
			});

			coordinator.selectProject("project-b");
			expect(coordinator.getSnapshot()).toEqual({
				projectId: "project-b",
				error: null,
				busy: true,
			});

			background.reject(new Error("background load failed"));
			await fetch;
			await settle();
			expect(requests).toBe(1);
			expect(coordinator.getSnapshot()).toEqual({
				projectId: "project-b",
				error: "background load failed",
				busy: false,
			});
		} finally {
			unsubscribe();
			queryClient.clear();
		}
	});

	it("adopts a destination project's pending success while fencing the old project's completion", async () => {
		const queryClient = new QueryClient({
			defaultOptions: { queries: { retry: false } },
		});
		const coordinator = new SessionListRequestCoordinator<SessionSummary[]>("project-a");
		const projectA = deferred<SessionSummary[]>();
		const projectB = deferred<SessionSummary[]>();
		const observers = [
			new QueryObserver(queryClient, {
				queryKey: queryKeys.sessions("project-a"),
				queryFn: () => coordinator.run("project-a", () => projectA.promise),
				enabled: false,
			}),
			new QueryObserver(queryClient, {
				queryKey: queryKeys.sessions("project-b"),
				queryFn: () => coordinator.run("project-b", () => projectB.promise),
				enabled: false,
			}),
		];
		const unsubscribes = observers.map((observer, index) =>
			observer.subscribe((result) => {
				coordinator.setQueryFetching(
					index === 0 ? "project-a" : "project-b",
					result.fetchStatus === "fetching",
				);
			}),
		);

		try {
			const oldFetch = observers[0].refetch();
			const destinationFetch = observers[1].refetch();
			await settle();
			coordinator.selectProject("project-b");
			expect(coordinator.getSnapshot()).toMatchObject({ error: null, busy: true });

			projectA.reject(new Error("stale old-project failure"));
			await oldFetch;
			await settle();
			expect(coordinator.getSnapshot()).toEqual({
				projectId: "project-b",
				error: null,
				busy: true,
			});

			projectB.resolve([session("project-b-session")]);
			await destinationFetch;
			await settle();
			expect(coordinator.getSnapshot()).toEqual({
				projectId: "project-b",
				error: null,
				busy: false,
			});
		} finally {
			for (const unsubscribe of unsubscribes) unsubscribe();
			queryClient.clear();
		}
	});

	it("keeps a no-data failure owned and busy through Retry until canonical success clears it", async () => {
		const coordinator = new SessionListRequestCoordinator<SessionSummary[]>("project-a");
		const failed = deferred<SessionSummary[]>();
		const first = coordinator.run("project-a", () => failed.promise);
		failed.reject(new Error("daemon unavailable"));
		await expect(first).rejects.toThrow("daemon unavailable");
		await settle();

		expect(coordinator.getSnapshot()).toEqual({
			projectId: "project-a",
			error: "daemon unavailable",
			busy: false,
		});

		const retried = deferred<SessionSummary[]>();
		let canonicalRequests = 0;
		const retry = coordinator.retry("project-a", () => {
			canonicalRequests += 1;
			return coordinator.run("project-a", () => retried.promise);
		});
		expect(coordinator.getSnapshot()).toMatchObject({ error: "daemon unavailable", busy: true });
		await settle();
		expect(canonicalRequests).toBe(1);
		expect(coordinator.getSnapshot()).toMatchObject({ error: "daemon unavailable", busy: true });

		retried.resolve([session("restored")]);
		await expect(retry).resolves.toEqual([session("restored")]);
		await settle();
		expect(coordinator.getSnapshot()).toMatchObject({ error: null, busy: false });
	});

	it("survives unrelated TanStack cache patches and only canonical fetch success clears it", async () => {
		const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false } } });
		const coordinator = new SessionListRequestCoordinator<SessionSummary[]>("project-a");
		const cached = session("cached");
		queryClient.setQueryData(queryKeys.sessions("project-a"), [cached]);

		const failure = deferred<SessionSummary[]>();
		const fetch = queryClient.fetchQuery({
			queryKey: queryKeys.sessions("project-a"),
			queryFn: () => coordinator.run("project-a", () => failure.promise),
			staleTime: 0,
		});
		failure.reject(new Error("refresh failed"));
		await expect(fetch).rejects.toThrow("refresh failed");
		await settle();

		queryClient.setQueryData<SessionSummary[]>(queryKeys.sessions("project-a"), (current) =>
			current?.map((item) => ({ ...item, metadata: { title: "patched" } })),
		);
		expect(queryClient.getQueryData<SessionSummary[]>(queryKeys.sessions("project-a"))?.[0].metadata.title).toBe("patched");
		expect(coordinator.getSnapshot().error).toBe("refresh failed");

		await coordinator.run("project-a", async () => [session("fresh")]);
		await settle();
		expect(coordinator.getSnapshot().error).toBeNull();
	});

	it("suppresses duplicate rapid Retry clicks and invokes one canonical refetch", async () => {
		const coordinator = new SessionListRequestCoordinator<SessionSummary[]>("project-a");
		const initial = coordinator.run("project-a", async () => {
			throw new Error("failed");
		});
		await expect(initial).rejects.toThrow("failed");
		await settle();

		const response = deferred<SessionSummary[]>();
		let refetches = 0;
		const refetch = () => {
			refetches += 1;
			return coordinator.run("project-a", () => response.promise);
		};
		const firstRetry = coordinator.retry("project-a", refetch);
		const secondRetry = coordinator.retry("project-a", refetch);
		await settle();

		expect(secondRetry).toBe(firstRetry);
		expect(refetches).toBe(1);
		response.resolve([]);
		await expect(firstRetry).resolves.toEqual([]);
	});

	it("fences stale success and failure after a project switch", async () => {
		const successCoordinator = new SessionListRequestCoordinator<SessionSummary[]>("project-a");
		const lateSuccess = deferred<SessionSummary[]>();
		const success = successCoordinator.run("project-a", () => lateSuccess.promise);
		successCoordinator.selectProject("project-b");
		lateSuccess.resolve([session("late")]);
		await expect(success).resolves.toHaveLength(1);
		await settle();
		expect(successCoordinator.getSnapshot()).toEqual({
			projectId: "project-b",
			error: null,
			busy: false,
		});

		const failureCoordinator = new SessionListRequestCoordinator<SessionSummary[]>("project-a");
		const lateFailure = deferred<SessionSummary[]>();
		const failure = failureCoordinator.run("project-a", () => lateFailure.promise);
		failureCoordinator.selectProject("project-b");
		lateFailure.reject(new Error("late failure"));
		await expect(failure).rejects.toThrow("late failure");
		await settle();
		expect(failureCoordinator.getSnapshot()).toEqual({
			projectId: "project-b",
			error: null,
			busy: false,
		});
	});
});
