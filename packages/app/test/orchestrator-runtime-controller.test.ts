import { describe, expect, it, vi } from "vitest";
import {
	createRelayOrchestratorRuntimeController,
} from "../src/orchestrator-runtime-controller.js";

describe("createRelayOrchestratorRuntimeController", () => {
	it("starts, flushes, and stops an injected shadow bridge in rust-shadow mode", async () => {
		const diagnostics: Array<{ type: "info" | "warning" | "error"; message: string }> = [];
		const controller = {
			start: vi.fn(async () => undefined),
			flush: vi.fn(async () => undefined),
			stop: vi.fn(async () => undefined),
		};
		const bridgeFactory = vi.fn(async () => controller);

		const runtimeController = await createRelayOrchestratorRuntimeController({
			orchestrator: { rootAgentId: "root" } as never,
			engineMode: "rust-shadow",
			diagnostics,
			bridgeFactory,
		});

		expect(runtimeController.authority).toBe("typescript");
		expect(runtimeController.shadowActive).toBe(true);
		expect(bridgeFactory).toHaveBeenCalledTimes(1);
		expect(controller.start).toHaveBeenCalledTimes(1);
		await runtimeController.flushShadow();
		expect(controller.flush).toHaveBeenCalledTimes(1);
		await runtimeController.stop();
		await runtimeController.stop();
		expect(controller.stop).toHaveBeenCalledTimes(1);
		expect(diagnostics).toEqual([]);
	});

	it("treats shadow bridge stop failures as warnings", async () => {
		const diagnostics: Array<{ type: "info" | "warning" | "error"; message: string }> = [];
		const controller = {
			start: vi.fn(async () => undefined),
			flush: vi.fn(async () => undefined),
			stop: vi.fn(async () => {
				throw new Error("stop failed");
			}),
		};

		const runtimeController = await createRelayOrchestratorRuntimeController({
			orchestrator: { rootAgentId: "root" } as never,
			engineMode: "rust-shadow",
			diagnostics,
			bridgeFactory: async () => controller,
		});

		await expect(runtimeController.stop()).resolves.toBeUndefined();
		expect(diagnostics).toEqual([
			{
				type: "warning",
				message:
					"Failed to stop the Rust orchestrator bridge cleanly for PI_RELAY_ORCH_ENGINE=rust-shadow: stop failed. TypeScript remains authoritative.",
			},
		]);
	});

	it("records bridge events and disconnects as runtime diagnostics", async () => {
		const diagnostics: Array<{ type: "info" | "warning" | "error"; message: string }> = [];
		let onEvent: ((event: any) => void) | undefined;
		let onDisconnect: ((reason?: Error) => void) | undefined;

		await createRelayOrchestratorRuntimeController({
			orchestrator: { rootAgentId: "root" } as never,
			engineMode: "rust-shadow",
			diagnostics,
			bridgeFactory: async (options) => {
				onEvent = options.onEvent;
				onDisconnect = options.onDisconnect;
				return {
					start: vi.fn(async () => {
						onEvent?.({
							type: "diagnostic",
							level: "warn",
							message: "shadow host ready",
						});
						onEvent?.({
							type: "shadow_diff",
							summary: "snapshot diverged",
							mismatchCount: 2,
						});
						onDisconnect?.(new Error("stdio closed"));
					}),
					flush: vi.fn(async () => undefined),
					stop: vi.fn(async () => undefined),
				};
			},
		});

		expect(diagnostics).toEqual([
			{ type: "warning", message: "Rust orchestrator bridge: shadow host ready" },
			{ type: "warning", message: "Rust orchestrator shadow diff: snapshot diverged (2 mismatches)." },
			{
				type: "warning",
				message: "Rust orchestrator bridge disconnected: stdio closed. TypeScript remains authoritative.",
			},
		]);
	});

	it("keeps TypeScript authoritative when no bridge factory is configured", async () => {
		const diagnostics: Array<{ type: "info" | "warning" | "error"; message: string }> = [];

		const runtimeController = await createRelayOrchestratorRuntimeController({
			orchestrator: { rootAgentId: "root" } as never,
			engineMode: "rust-shadow",
			diagnostics,
		});

		expect(runtimeController.shadowActive).toBe(false);
		expect(diagnostics).toEqual([
			{
				type: "warning",
				message:
					"PI_RELAY_ORCH_ENGINE=rust-shadow selected, but no orchestrator bridge factory is configured; continuing with the TypeScript orchestrator as the only authority.",
			},
		]);
	});

	it("surfaces startup failures and falls back to a no-op controller", async () => {
		const diagnostics: Array<{ type: "info" | "warning" | "error"; message: string }> = [];
		const stop = vi.fn(async () => undefined);

		const runtimeController = await createRelayOrchestratorRuntimeController({
			orchestrator: { rootAgentId: "root" } as never,
			engineMode: "rust",
			diagnostics,
			bridgeFactory: async () => ({
				start: vi.fn(async () => {
					throw new Error("bridge boot failed");
				}),
				flush: vi.fn(async () => undefined),
				stop,
			}),
		});

		expect(runtimeController.shadowActive).toBe(false);
		expect(stop).toHaveBeenCalledTimes(1);
		expect(diagnostics).toEqual([
			{
				type: "info",
				message:
					"PI_RELAY_ORCH_ENGINE=rust is not authoritative yet on this branch; continuing to mirror the TypeScript orchestrator through the Rust bridge lifecycle only.",
			},
			{
				type: "error",
				message:
					"Failed to start the Rust orchestrator bridge for PI_RELAY_ORCH_ENGINE=rust: bridge boot failed. Continuing with the TypeScript orchestrator as the only authority.",
			},
		]);
	});
});
