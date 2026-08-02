import { describe, expect, it } from "vitest";
import { TurnDetailRequestCoordinator, type TurnDetailRequestToken } from "./turnDetailRequestCoordinator.ts";

interface Deferred<T> {
	promise: Promise<T>;
	resolve(value: T): void;
}

function deferred<T>(): Deferred<T> {
	let resolve!: (value: T) => void;
	const promise = new Promise<T>((resolvePromise) => {
		resolve = resolvePromise;
	});
	return { promise, resolve };
}

async function complete(
	coordinator: TurnDetailRequestCoordinator,
	token: TurnDetailRequestToken,
	response: Deferred<string>,
): Promise<[string, boolean]> {
	return [await response.promise, coordinator.isCurrent(token)];
}

describe("TurnDetailRequestCoordinator", () => {
	for (const completionOrder of [["current", "historical"], ["historical", "current"]] as const) {
		it(`keeps current auto and historical manual requests independent when ${completionOrder[0]} completes first`, async () => {
			const coordinator = new TurnDetailRequestCoordinator();
			const tokens = {
				current: coordinator.begin("session-1", "current-card"),
				historical: coordinator.begin("session-1", "historical-card"),
			};
			const responses = {
				current: deferred<string>(),
				historical: deferred<string>(),
			};
			const completions = {
				current: complete(coordinator, tokens.current, responses.current),
				historical: complete(coordinator, tokens.historical, responses.historical),
			};

			responses[completionOrder[0]].resolve(completionOrder[0]);
			expect(await completions[completionOrder[0]]).toEqual([completionOrder[0], true]);
			responses[completionOrder[1]].resolve(completionOrder[1]);
			expect(await completions[completionOrder[1]]).toEqual([completionOrder[1], true]);
		});

		it(`keeps two historical card requests independent when ${completionOrder[0]} completes first`, async () => {
			const coordinator = new TurnDetailRequestCoordinator();
			const tokens = {
				current: coordinator.begin("session-1", "historical-card-a"),
				historical: coordinator.begin("session-1", "historical-card-b"),
			};
			const responses = {
				current: deferred<string>(),
				historical: deferred<string>(),
			};
			const completions = {
				current: complete(coordinator, tokens.current, responses.current),
				historical: complete(coordinator, tokens.historical, responses.historical),
			};

			responses[completionOrder[0]].resolve(completionOrder[0]);
			expect(await completions[completionOrder[0]]).toEqual([completionOrder[0], true]);
			responses[completionOrder[1]].resolve(completionOrder[1]);
			expect(await completions[completionOrder[1]]).toEqual([completionOrder[1], true]);
		});
	}

	it("invalidates only an older request for the same session card", () => {
		const coordinator = new TurnDetailRequestCoordinator();
		const old = coordinator.begin("session-1", "card-a");
		const other = coordinator.begin("session-1", "card-b");
		const current = coordinator.begin("session-1", "card-a");

		expect(coordinator.isCurrent(old)).toBe(false);
		expect(coordinator.isCurrent(other)).toBe(true);
		expect(coordinator.isCurrent(current)).toBe(true);
	});
});
