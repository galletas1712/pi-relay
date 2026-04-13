import type { AgentMessage } from "./types.js";

export type MailboxItemKind = "steering" | "follow_up" | "tool_result";

export interface MailboxItem {
	kind: MailboxItemKind;
	message: AgentMessage;
}
