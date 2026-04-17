export { LocalClient, type LocalClientInternals } from "./local-client.js";
export {
	decodeMessage,
	encodeMessage,
	type MethodMap,
	type NodeIO,
	RpcClient,
	type RpcErrorPayload,
	type RpcMessage,
	type RpcMethod,
	type RpcParams,
	type RpcResult,
	RpcServer,
	type WireSessionSummary,
} from "./rpc/index.js";
export type {
	AuthStatus,
	Client,
	ModelCycleResult,
	ModelInfo,
	OpenSessionOptions,
	PromptOptions,
	ResumeOptions,
	SessionEvent,
	SessionHandle,
	SessionState,
	SessionStats,
	SessionSummary,
} from "./types.js";
