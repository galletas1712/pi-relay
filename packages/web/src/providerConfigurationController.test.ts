import { describe, expect, it, vi } from "vitest";
import type { ConfigureSessionResult } from "./agentApi.ts";
import {
	ProviderConfigurationController,
	type ProviderConfigurationControllerOptions,
} from "./providerConfigurationController.ts";
import {
	providerFromModelKey,
	withReasoningEffort,
} from "./sessionDefaults.ts";
import type { ProviderConfig } from "./types.ts";

function deferred<T>() {
	let resolve!: (value: T) => void;
	let reject!: (error: unknown) => void;
	const promise = new Promise<T>((resolvePromise, rejectPromise) => {
		resolve = resolvePromise;
		reject = rejectPromise;
	});
	return { promise, resolve, reject };
}

function result(provider: ProviderConfig): ConfigureSessionResult {
	return {
		session_id: "session",
		activity: "idle",
		provider,
		metadata: {},
	};
}

function controller(overrides: Partial<ProviderConfigurationControllerOptions> = {}) {
	const options: ProviderConfigurationControllerOptions = {
		configure: vi.fn(async (_target, provider) => result(provider)),
		commit: vi.fn(),
		fail: vi.fn(),
		change: vi.fn(),
		...overrides,
	};
	return { controller: new ProviderConfigurationController(options), options };
}

const base: ProviderConfig = {
	kind: "openai",
	model: "gpt-5.6-sol",
	reasoning_effort: "medium",
	max_tokens: 4096,
	prompt_cache: { store: false },
};

describe("ProviderConfigurationController", () => {
	it("serializes rapid edits and sends only the latest coalesced provider", async () => {
		const first = deferred<ConfigureSessionResult>();
		const second = deferred<ConfigureSessionResult>();
		const configure = vi
			.fn()
			.mockImplementationOnce(() => first.promise)
			.mockImplementationOnce(() => second.promise);
		const { controller: subject, options } = controller({ configure });
		const target = { sessionId: "one", projectId: "project" };

		subject.update(target, base, (provider) => withReasoningEffort(provider, "high"), "reasoning effort");
		const settled = subject.settled("one");
		subject.update(target, base, (provider) => withReasoningEffort(provider, "low"), "reasoning effort");
		expect(configure).toHaveBeenCalledTimes(1);
		expect(subject.desired("one")?.reasoning_effort).toBe("low");

		first.resolve(result(withReasoningEffort(base, "high")));
		await first.promise;
		await vi.waitFor(() => expect(configure).toHaveBeenCalledTimes(2));
		expect(configure.mock.calls[1]?.[1]).toEqual(withReasoningEffort(base, "low"));
		second.resolve(result(withReasoningEffort(base, "low")));
		await settled;
		expect(options.commit).toHaveBeenCalledTimes(2);
		expect(subject.desired("one")).toBeNull();
	});

	it("fences immutable targets while independent sessions settle out of order", async () => {
		const requests = new Map<string, ReturnType<typeof deferred<ConfigureSessionResult>>>();
		const configure = vi.fn((target: { sessionId: string }) => {
			const request = deferred<ConfigureSessionResult>();
			requests.set(target.sessionId, request);
			return request.promise;
		});
		const { controller: subject, options } = controller({ configure });
		subject.update(
			{ sessionId: "one", projectId: "first-project" },
			base,
			(provider) => withReasoningEffort(provider, "high"),
			"reasoning effort",
		);
		subject.update(
			{ sessionId: "two", projectId: "second-project" },
			base,
			(provider) => withReasoningEffort(provider, "low"),
			"reasoning effort",
		);
		const oneSettled = subject.settled("one");
		const twoSettled = subject.settled("two");
		requests.get("two")!.resolve(result(withReasoningEffort(base, "low")));
		await twoSettled;
		requests.get("one")!.resolve(result(withReasoningEffort(base, "high")));
		await oneSettled;
		expect(options.commit).toHaveBeenNthCalledWith(
			1,
			{ sessionId: "two", projectId: "second-project" },
			expect.objectContaining({ reasoning_effort: "low" }),
			expect.any(Object),
		);
		expect(options.commit).toHaveBeenNthCalledWith(
			2,
			{ sessionId: "one", projectId: "first-project" },
			expect.objectContaining({ reasoning_effort: "high" }),
			expect.any(Object),
		);
	});

	it("reports the latest failed edit and clears its optimistic provider", async () => {
		const failure = new Error("rejected");
		const { controller: subject, options } = controller({
			configure: vi.fn(async () => {
				throw failure;
			}),
		});
		const target = { sessionId: "one", projectId: null };
		subject.update(target, base, (provider) => withReasoningEffort(provider, "max"), "reasoning effort");
		await subject.settled("one");
		expect(options.fail).toHaveBeenCalledWith(target, "reasoning effort", failure);
		expect(subject.desired("one")).toBeNull();
	});

	it("composes model and effort edits without dropping complete provider fields", async () => {
		const first = deferred<ConfigureSessionResult>();
		const second = deferred<ConfigureSessionResult>();
		const requested: ProviderConfig[] = [];
		const configure = vi.fn((_target, provider: ProviderConfig) => {
			requested.push(provider);
			return requested.length === 1 ? first.promise : second.promise;
		});
		const { controller: subject } = controller({ configure });
		const target = { sessionId: "one", projectId: null };
		subject.update(
			target,
			base,
			(provider) => providerFromModelKey("openai:gpt-5.6-terra", provider),
			"model",
		);
		subject.update(target, base, (provider) => withReasoningEffort(provider, "low"), "reasoning effort");
		expect(subject.desired("one")).toEqual({
			...base,
			model: "gpt-5.6-terra",
			reasoning_effort: "low",
		});
		const settled = subject.settled("one");
		first.resolve(result({
			...requested[0]!,
			max_tokens: 8192,
			prompt_cache: { store: true, key: "canonical" },
		}));
		await vi.waitFor(() => expect(configure).toHaveBeenCalledTimes(2));
		expect(requested[1]).toEqual({
			...base,
			model: "gpt-5.6-terra",
			reasoning_effort: "low",
			max_tokens: 8192,
			prompt_cache: { store: true, key: "canonical" },
		});
		second.resolve(result(requested[1]!));
		await settled;
	});

	it("settles an in-flight request after disposal without publishing callbacks", async () => {
		const request = deferred<ConfigureSessionResult>();
		const { controller: subject, options } = controller({
			configure: vi.fn(() => request.promise),
		});
		subject.update(
			{ sessionId: "one", projectId: null },
			base,
			(provider) => withReasoningEffort(provider, "high"),
			"reasoning effort",
		);
		const settled = subject.settled("one");
		subject.dispose();
		request.resolve(result(withReasoningEffort(base, "high")));
		await settled;
		expect(options.commit).not.toHaveBeenCalled();
		expect(options.fail).not.toHaveBeenCalled();
		expect(subject.desired("one")).toBeNull();
	});
});
