// M11a: comms.list — inter-agent message visibility (contract v0.2,
// additive). Sources: the session's prime-comms outbox (outbound, folded to
// terminal status per message id) + the delivery custom entries persisted
// into this session's JSONL on receipt (inbound). Live deliveries ALSO flow
// as comms.message contract events (prime-comms appendEntry → entry_appended).
import * as agentfiles from "../agentfiles.ts";
import { requireString, type MethodTable, type Params } from "./common.ts";

export function commsMethods(): MethodTable {
	return {
		async "comms.list"(_conn, params: Params) {
			const sessionId = requireString(params, "sessionId");
			const { sessionFile, sessionDir } = await agentfiles.sessionFileInfo(sessionId);
			const messages = [
				...agentfiles.inboundComms(sessionFile, sessionId),
				...agentfiles.outboundComms(sessionDir, sessionId),
			].sort((a, b) => (a.ts < b.ts ? -1 : a.ts > b.ts ? 1 : 0));
			return { sessionId, messages };
		},
	};
}
