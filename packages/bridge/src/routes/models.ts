// M11a: model surface routes (contract v0.2, additive).
// models.list — catalog + availability + auth status from the pi ModelRuntime
// probe on the bridge agentDir (supervisor/../models.ts). session.setModel /
// session.setThinkingLevel — rpc passthrough with typed errors + session.model
// event emission (supervisor).
import * as models from "../models.ts";
import * as supervisor from "../supervisor.ts";
import { BridgeError } from "../supervisor.ts";
import { requireString, type MethodTable, type Params } from "./common.ts";

const PROVIDER_ID_MAX = 200;
const MODEL_ID_MAX = 300;
const THINKING_LEVELS = new Set(["off", "minimal", "low", "medium", "high", "xhigh", "max"]);

export function modelMethods(): MethodTable {
	return {
		async "models.list"(_conn, params: Params) {
			const refresh = params.refresh === true;
			return models.listModels({ refresh });
		},
		async "session.setModel"(_conn, params: Params) {
			const sessionId = requireString(params, "sessionId");
			const provider = requireString(params, "provider");
			const modelId = requireString(params, "modelId");
			if (provider.length > PROVIDER_ID_MAX || modelId.length > MODEL_ID_MAX) {
				throw new BridgeError("bad_request", "provider/modelId too long");
			}
			return supervisor.setSessionModel(sessionId, provider, modelId);
		},
		async "session.setThinkingLevel"(_conn, params: Params) {
			const sessionId = requireString(params, "sessionId");
			const level = requireString(params, "level");
			if (!THINKING_LEVELS.has(level)) {
				throw new BridgeError("bad_request", `params.level must be one of ${[...THINKING_LEVELS].join("|")}`);
			}
			return supervisor.setSessionThinkingLevel(sessionId, level);
		},
	};
}
