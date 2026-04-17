import { Container } from "@mariozechner/pi-tui";
import { describe, expect, test, vi } from "vitest";
import { InteractiveMode } from "../src/modes/interactive/interactive-mode.js";

function createLoaderStub() {
	return {
		stop: vi.fn(),
		setMessage: vi.fn(),
		render: () => [],
		invalidate: () => {},
	};
}

describe("InteractiveMode retry loader handling", () => {
	test("restores the working loader after retry cleanup when a run is still active", async () => {
		const loadingAnimation = createLoaderStub();
		const retryLoader = createLoaderStub();
		const retryCountdown = { dispose: vi.fn() };
		const statusContainer = new Container();
		statusContainer.addChild(loadingAnimation);

		const fakeThis = {
			isInitialized: true,
			session: {
				agent: {
					state: {
						isStreaming: true,
						streamingMessage: undefined,
						pendingToolCalls: new Set(),
						messages: [{ role: "user" }],
					},
				},
			},
			footer: { invalidate: vi.fn() },
			defaultEditor: { onEscape: vi.fn() },
			retryEscapeHandler: vi.fn(),
			retryCountdown,
			retryLoader,
			autoCompactionLoader: undefined,
			loadingAnimation,
			pendingWorkingMessage: undefined,
			pendingTools: new Map(),
			statusContainer,
			ui: { requestRender: vi.fn() },
			showError: vi.fn(),
			shouldShowWorkingAnimation: Reflect.get(InteractiveMode.prototype, "shouldShowWorkingAnimation"),
			syncLoadingAnimationWithSession: Reflect.get(InteractiveMode.prototype, "syncLoadingAnimationWithSession"),
		};

		const handleEvent = Reflect.get(InteractiveMode.prototype, "handleEvent") as (
			this: typeof fakeThis,
			event: { type: "auto_retry_end"; success: boolean; attempt: number; finalError?: string },
		) => Promise<void>;

		await handleEvent.call(fakeThis, {
			type: "auto_retry_end",
			success: true,
			attempt: 1,
		});

		expect(retryLoader.stop).toHaveBeenCalledTimes(1);
		expect(retryCountdown.dispose).toHaveBeenCalledTimes(1);
		expect(fakeThis.statusContainer.children).toContain(loadingAnimation);
		expect(fakeThis.loadingAnimation).toBe(loadingAnimation);
		expect(fakeThis.ui.requestRender).toHaveBeenCalledTimes(1);
	});

	test("hides the working loader when the session is only waiting after an assistant turn", () => {
		const loadingAnimation = createLoaderStub();
		const statusContainer = new Container();
		statusContainer.addChild(loadingAnimation);

		const fakeThis = {
			session: {
				agent: {
					state: {
						isStreaming: true,
						streamingMessage: undefined,
						pendingToolCalls: new Set(),
						messages: [{ role: "assistant" }],
					},
				},
			},
			retryLoader: undefined,
			autoCompactionLoader: undefined,
			loadingAnimation,
			pendingWorkingMessage: undefined,
			pendingTools: new Map(),
			statusContainer,
			ui: {},
			shouldShowWorkingAnimation: Reflect.get(InteractiveMode.prototype, "shouldShowWorkingAnimation"),
		};

		const syncLoadingAnimationWithSession = Reflect.get(
			InteractiveMode.prototype,
			"syncLoadingAnimationWithSession",
		) as (this: typeof fakeThis) => void;

		syncLoadingAnimationWithSession.call(fakeThis);

		expect(loadingAnimation.stop).toHaveBeenCalledTimes(1);
		expect(fakeThis.loadingAnimation).toBeUndefined();
		expect(fakeThis.statusContainer.children).toHaveLength(0);
	});
});
