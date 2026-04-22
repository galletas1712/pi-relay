/**
 * Core modules shared between all run modes.
 */

export {
	AgentSession,
	type AgentSessionConfig,
	type AgentSessionEvent,
	type AgentSessionEventListener,
	type ModelCycleResult,
	type PromptOptions,
	type SessionStats,
} from "./agent-session.js";
export {
	AgentSessionRuntime,
	type CreateAgentSessionRuntimeFactory,
	type CreateAgentSessionRuntimeResult,
	createAgentSessionRuntime,
} from "./agent-session-runtime.js";
export {
	type AgentSessionRuntimeDiagnostic,
	type AgentSessionServices,
	type CreateAgentSessionFromServicesOptions,
	type CreateAgentSessionServicesOptions,
	createAgentSessionFromServices,
	createAgentSessionServices,
} from "./agent-session-services.js";
export { type BashExecutorOptions, type BashResult, executeBash, executeBashWithOperations } from "./bash-executor.js";
export type { CompactionResult } from "./compaction/index.js";
export { createEventBus, type EventBus, type EventBusController } from "./event-bus.js";
export {
	attachSessionShadowBridge,
	SessionShadowBridgeClient,
	type SessionShadowBridgeClientOptions,
	type SessionShadowBridgeController,
	type SessionShadowBridgeIO,
} from "./session-shadow/client.js";
export {
	createSessionShadowSnapshot,
	decodeSessionShadowBridgeMessage,
	encodeSessionShadowBridgeMessage,
	SESSION_CORE_BRIDGE_PROTOCOL_VERSION,
	type SessionShadowBridgeAck,
	type SessionShadowBridgeCallMessage,
	type SessionShadowBridgeCommand,
	type SessionShadowBridgeError,
	type SessionShadowBridgeErrorMessage,
	type SessionShadowBridgeEvent,
	type SessionShadowBridgeEventMessage,
	type SessionShadowBridgeMessage,
	type SessionShadowBridgeResultMessage,
	type SessionShadowBridgeMode,
	type SessionShadowCommandAppliedEvent,
	type SessionShadowDiagnosticEvent,
	type SessionShadowDispatchCommand,
	type SessionShadowDisposeCommand,
	type SessionShadowHelloCommand,
	type SessionShadowSnapshot,
	type SessionShadowStateSyncedEvent,
	type SessionShadowSyncReason,
	type SessionShadowSyncStateCommand,
} from "./session-shadow/codec.js";
// Extensions system
export {
	type AgentEndEvent,
	type AgentStartEvent,
	type AgentToolResult,
	type AgentToolUpdateCallback,
	type BeforeAgentStartEvent,
	type ContextEvent,
	defineTool,
	discoverAndLoadExtensions,
	type ExecOptions,
	type ExecResult,
	type Extension,
	type ExtensionAPI,
	type ExtensionCommandContext,
	type ExtensionContext,
	type ExtensionError,
	type ExtensionEvent,
	type ExtensionFactory,
	type ExtensionFlag,
	type ExtensionHandler,
	ExtensionRunner,
	type ExtensionShortcut,
	type ExtensionUIContext,
	type LoadExtensionsResult,
	type MessageRenderer,
	type RegisteredCommand,
	type SessionBeforeCompactEvent,
	type SessionBeforeForkEvent,
	type SessionBeforeSwitchEvent,
	type SessionBeforeTreeEvent,
	type SessionCompactEvent,
	type SessionShutdownEvent,
	type SessionStartEvent,
	type SessionTreeEvent,
	type ToolCallEvent,
	type ToolCallEventResult,
	type ToolDefinition,
	type ToolRenderResultOptions,
	type ToolResultEvent,
	type TurnEndEvent,
	type TurnStartEvent,
} from "./extensions/index.js";
export { createSyntheticSourceInfo } from "./source-info.js";
