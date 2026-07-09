import { QueryClient, QueryObserver } from "@tanstack/react-query";
import { describe, expect, it, vi } from "vitest";
import {
	DelegationListRetryController,
	type DelegationListRetryScope,
} from "./delegationListRetryController.ts";
import { queryKeys } from "./queryKeys.ts";
import type { DelegationListResult } from "./types.ts";

function deferred<T>() {
	let resolve!: (value: T) => void;
	let reject!: (reason: unknown) => void;
	const promise = new Promise<T>((resolvePromise, rejectPromise) => {
		resolve = resolvePromise;
		reject = rejectPromise;
	});
	return { promise, resolve, reject };
}

function page(
	label: string,
	{ parentSessionId = "parent-1", limit = 3 }: Partial<DelegationListRetryScope> = {},
): DelegationListResult {
	return {
		parent_session_id: parentSessionId ?? "",
		limit,
		has_more: false,
		delegations: [{
			delegation_id: label,
			kind: "full",
			status: "done",
			workflow: null,
			label,
			progress: { expected: 1, spawned: 1, terminal: 1, running: 0, failed: 0 },
			subagents: [],
		}],
	};
}

async function settle(): Promise<void> {
	await Promise.resolve();
	await Promise.resolve();
	await Promise.resolve();
}

describe("delegation list retry controller", () => {
	it.each([
		{ name: "initial error", cached: false },
		{ name: "cached refresh error", cached: true },
	])("synchronously locks duplicate real QueryObserver refetches after $name", async ({ cached }) => {
		const queryClient = new QueryClient({
			defaultOptions: { queries: { retry: false } },
		});
		const retryRequest = deferred<DelegationListResult>();
		const queryFn = vi.fn<() => Promise<DelegationListResult>>();
		if (cached) queryFn.mockResolvedValueOnce(page("cached"));
		queryFn
			.mockRejectedValueOnce(new Error("list failed"))
			.mockImplementationOnce(() => retryRequest.promise);
		const observer = new QueryObserver(queryClient, {
			queryKey: queryKeys.delegations("parent-1", 3),
			queryFn,
			enabled: false,
		});
		const unsubscribe = observer.subscribe(() => undefined);
		const controller = new DelegationListRetryController();
		const scope = { parentSessionId: "parent-1", limit: 3 };

		try {
			if (cached) await observer.refetch();
			await observer.refetch();
			expect(observer.getCurrentResult().isError).toBe(true);
			if (cached) expect(observer.getCurrentResult().data).toEqual(page("cached"));

			const callsBeforeRetry = queryFn.mock.calls.length;
			const first = controller.retry(scope, () => observer.refetch());
			const second = controller.retry({ ...scope }, () => observer.refetch());
			await settle();

			expect(first).toBe(second);
			expect(controller.isPending(scope)).toBe(true);
			expect(queryFn).toHaveBeenCalledTimes(callsBeforeRetry + 1);

			retryRequest.resolve(page("restored"));
			await first;
			expect(controller.isPending(scope)).toBe(false);
			expect(observer.getCurrentResult().data).toEqual(page("restored"));
		} finally {
			unsubscribe();
			queryClient.clear();
		}
	});

	it.each([
		{
			name: "different limits for one parent",
			firstScope: { parentSessionId: "parent-1", limit: 3 },
			secondScope: { parentSessionId: "parent-1", limit: 100 },
		},
		{
			name: "different parents at one limit",
			firstScope: { parentSessionId: "parent-1", limit: 3 },
			secondScope: { parentSessionId: "parent-2", limit: 3 },
		},
	])(
		"runs $name independently while coalescing each exact scope",
		async ({ firstScope, secondScope }) => {
			const queryClient = new QueryClient({
				defaultOptions: { queries: { retry: false } },
			});
			const firstRequest = deferred<DelegationListResult>();
			const firstRequestAfterCleanup = deferred<DelegationListResult>();
			const secondRequest = deferred<DelegationListResult>();
			const secondRequestAfterCleanup = deferred<DelegationListResult>();
			const firstQueryFn = vi.fn<() => Promise<DelegationListResult>>()
				.mockImplementationOnce(() => firstRequest.promise)
				.mockImplementationOnce(() => firstRequestAfterCleanup.promise);
			const secondQueryFn = vi.fn<() => Promise<DelegationListResult>>()
				.mockImplementationOnce(() => secondRequest.promise)
				.mockImplementationOnce(() => secondRequestAfterCleanup.promise);
			const firstObserver = new QueryObserver(queryClient, {
				queryKey: queryKeys.delegations(
					firstScope.parentSessionId,
					firstScope.limit,
				),
				queryFn: firstQueryFn,
				enabled: false,
			});
			const secondObserver = new QueryObserver(queryClient, {
				queryKey: queryKeys.delegations(
					secondScope.parentSessionId,
					secondScope.limit,
				),
				queryFn: secondQueryFn,
				enabled: false,
			});
			const unsubscribeFirst = firstObserver.subscribe(() => undefined);
			const unsubscribeSecond = secondObserver.subscribe(() => undefined);
			const controller = new DelegationListRetryController();
			const firstRefetch = () => firstObserver.refetch({ throwOnError: true });
			const secondRefetch = () => secondObserver.refetch({ throwOnError: true });

			try {
				const first = controller.retry(firstScope, firstRefetch);
				const firstDuplicate = controller.retry({ ...firstScope }, firstRefetch);
				const second = controller.retry(secondScope, secondRefetch);
				const secondDuplicate = controller.retry({ ...secondScope }, secondRefetch);
				await settle();

				expect(firstDuplicate).toBe(first);
				expect(secondDuplicate).toBe(second);
				expect(second).not.toBe(first);
				expect(firstQueryFn).toHaveBeenCalledTimes(1);
				expect(secondQueryFn).toHaveBeenCalledTimes(1);
				expect(controller.isPending(firstScope)).toBe(true);
				expect(controller.isPending(secondScope)).toBe(true);

				firstRequest.resolve(page("first restored", firstScope));
				await first;
				expect(controller.isPending(firstScope)).toBe(false);
				expect(controller.isPending(secondScope)).toBe(true);

				secondRequest.reject(new Error("second retry failed"));
				await expect(second).rejects.toThrow("second retry failed");
				expect(controller.isPending(secondScope)).toBe(false);

				const firstAfterCleanup = controller.retry(firstScope, firstRefetch);
				const secondAfterCleanup = controller.retry(secondScope, secondRefetch);
				expect(firstAfterCleanup).not.toBe(first);
				expect(secondAfterCleanup).not.toBe(second);
				await settle();
				expect(firstQueryFn).toHaveBeenCalledTimes(2);
				expect(secondQueryFn).toHaveBeenCalledTimes(2);

				firstRequestAfterCleanup.resolve(page("first again", firstScope));
				secondRequestAfterCleanup.resolve(page("second restored", secondScope));
				await Promise.all([firstAfterCleanup, secondAfterCleanup]);
				expect(controller.isPending(firstScope)).toBe(false);
				expect(controller.isPending(secondScope)).toBe(false);
			} finally {
				unsubscribeFirst();
				unsubscribeSecond();
				queryClient.clear();
			}
		},
	);
});
