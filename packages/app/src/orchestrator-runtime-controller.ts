import type { AgentSessionRuntimeDiagnostic } from "@pi-relay/coding-agent";
import type {
	Orchestrator,
	OrchestratorShadowBridgeController,
	RelayCoreBridgeEvent,
} from "@pi-relay/orchestrator";

export type RelayOrchestratorEngineMode = "legacy" | "ts-core" | "rust-shadow" | "rust";
export type RelayOrchestratorBridgeMode = Extract<RelayOrchestratorEngineMode, "rust-shadow" | "rust">;

export interface RelayOrchestratorRuntimeController {
	readonly authority: "typescript";
	readonly engineMode: RelayOrchestratorEngineMode;
	readonly shadowActive: boolean;
	flushShadow(): Promise<void>;
	stop(): Promise<void>;
}

export interface RelayOrchestratorBridgeFactoryOptions {
	orchestrator: Orchestrator;
	engineMode: RelayOrchestratorBridgeMode;
	onEvent: (event: RelayCoreBridgeEvent) => void;
	onDisconnect: (reason?: Error) => void;
}

export type RelayOrchestratorBridgeFactory = (
	options: RelayOrchestratorBridgeFactoryOptions,
) => Promise<OrchestratorShadowBridgeController | undefined> | OrchestratorShadowBridgeController | undefined;

function createNoopController(engineMode: RelayOrchestratorEngineMode): RelayOrchestratorRuntimeController {
	return {
		authority: "typescript",
		engineMode,
		shadowActive: false,
		async flushShadow() {
			// no-op until a Rust bridge is attached
		},
		async stop() {
			// no-op until a Rust bridge is attached
		},
	};
}

function formatError(error: unknown): string {
	if (error instanceof Error) {
		return error.message;
	}
	return String(error);
}

function formatBridgeStopFailureMessage(
	engineMode: RelayOrchestratorBridgeMode,
	error: unknown,
): string {
	return `Failed to stop the Rust orchestrator bridge cleanly for PI_RELAY_ORCH_ENGINE=${engineMode}: ${formatError(error)}. TypeScript remains authoritative.`;
}

function formatBridgeEventMessage(event: RelayCoreBridgeEvent): string {
	if (event.type === "diagnostic") {
		return `Rust orchestrator bridge: ${event.message}`;
	}
	return `Rust orchestrator shadow diff: ${event.summary} (${event.mismatchCount} mismatch${event.mismatchCount === 1 ? "" : "es"}).`;
}

function toDiagnosticLevel(event: RelayCoreBridgeEvent): AgentSessionRuntimeDiagnostic["type"] {
	if (event.type === "diagnostic") {
		if (event.level === "error") {
			return "error";
		}
		if (event.level === "warn") {
			return "warning";
		}
		return "info";
	}
	return event.mismatchCount > 0 ? "warning" : "info";
}

function pushDiagnostic(
	diagnostics: AgentSessionRuntimeDiagnostic[],
	type: AgentSessionRuntimeDiagnostic["type"],
	message: string,
): void {
	diagnostics.push({ type, message });
}

export async function createRelayOrchestratorRuntimeController(options: {
	orchestrator: Orchestrator;
	engineMode: RelayOrchestratorEngineMode;
	diagnostics: AgentSessionRuntimeDiagnostic[];
	bridgeFactory?: RelayOrchestratorBridgeFactory;
}): Promise<RelayOrchestratorRuntimeController> {
	const { orchestrator, engineMode, diagnostics, bridgeFactory } = options;
	if (engineMode !== "rust-shadow" && engineMode !== "rust") {
		return createNoopController(engineMode);
	}

	if (engineMode === "rust") {
		pushDiagnostic(
			diagnostics,
			"info",
			"PI_RELAY_ORCH_ENGINE=rust is not authoritative yet on this branch; continuing to mirror the TypeScript orchestrator through the Rust bridge lifecycle only.",
		);
	}

	if (!bridgeFactory) {
		pushDiagnostic(
			diagnostics,
			"warning",
			`PI_RELAY_ORCH_ENGINE=${engineMode} selected, but no orchestrator bridge factory is configured; continuing with the TypeScript orchestrator as the only authority.`,
		);
		return createNoopController(engineMode);
	}

	let controller: OrchestratorShadowBridgeController | undefined;
	try {
		controller = await bridgeFactory({
			orchestrator,
			engineMode,
			onEvent(event) {
				pushDiagnostic(diagnostics, toDiagnosticLevel(event), formatBridgeEventMessage(event));
			},
			onDisconnect(reason) {
				pushDiagnostic(
					diagnostics,
					"warning",
					`Rust orchestrator bridge disconnected${reason ? `: ${reason.message}` : ""}. TypeScript remains authoritative.`,
				);
			},
		});
		if (!controller) {
			pushDiagnostic(
				diagnostics,
				"warning",
				`PI_RELAY_ORCH_ENGINE=${engineMode} selected, but the orchestrator bridge factory returned no controller; continuing with the TypeScript orchestrator as the only authority.`,
			);
			return createNoopController(engineMode);
		}
		await controller.start();
		let stopped = false;
		return {
			authority: "typescript",
			engineMode,
			shadowActive: true,
			async flushShadow() {
				if (stopped) {
					return;
				}
				await controller.flush();
			},
			async stop() {
				if (stopped) {
					return;
				}
				stopped = true;
				try {
					await controller.stop();
				} catch (error) {
					pushDiagnostic(diagnostics, "warning", formatBridgeStopFailureMessage(engineMode, error));
				}
			},
		};
	} catch (error) {
		try {
			await controller?.stop();
		} catch (stopError) {
			pushDiagnostic(diagnostics, "warning", formatBridgeStopFailureMessage(engineMode, stopError));
		}
		pushDiagnostic(
			diagnostics,
			"error",
			`Failed to start the Rust orchestrator bridge for PI_RELAY_ORCH_ENGINE=${engineMode}: ${formatError(error)}. Continuing with the TypeScript orchestrator as the only authority.`,
		);
		return createNoopController(engineMode);
	}
}

export async function stopRelayOrchestratorRuntimeController(
	controller: RelayOrchestratorRuntimeController | undefined,
): Promise<void> {
	try {
		await controller?.stop();
	} catch {
		// Controllers should already translate stop failures into diagnostics,
		// but shutdown/rebuild must remain best-effort even if one escapes.
	}
}
