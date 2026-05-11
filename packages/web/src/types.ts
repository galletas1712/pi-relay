export type Activity = "idle" | "queued" | "running";

export interface ProviderConfig {
	kind: string;
	model: string;
	max_tokens?: number;
	prompt_cache?: Record<string, unknown>;
}

export interface SessionSummary {
	session_id: string;
	activity: Activity;
	active_leaf_id: string | null;
	provider: ProviderConfig;
	metadata: Record<string, unknown>;
	updated_at: string;
}

export interface PendingAction {
	action_row_id: string;
	kind: "model" | "tool" | "compaction" | string;
	status: string;
	payload: Record<string, unknown>;
}

export interface QueuedInput {
	input_id: string;
	priority: "follow_up" | "steer" | string;
	status: "queued" | "consuming" | string;
	content: ContentBlock[];
	client_input_id?: string | null;
	created_at: string;
	promoted_at?: string | null;
}

export interface SessionSnapshot {
	session_id: string;
	activity: Activity;
	active_leaf_id: string | null;
	provider: ProviderConfig;
	metadata: Record<string, unknown>;
	pending_actions: PendingAction[];
	queued_inputs: QueuedInput[];
	last_event_id: number;
}

export interface DaemonConfig {
	system_prompt: string | null;
}

export interface EventFrame {
	event_id: number;
	event: string;
	session_id: string;
	data: Record<string, unknown>;
}

export type ContentBlock =
	| { type: "text"; text: string }
	| {
			type: "image";
			image: {
				mime_type: string;
				source: { kind: "url" | "base64"; value: string };
			};
	  };

export interface UserMessage {
	content: ContentBlock[];
}

export type AssistantItem =
	| { type: "text"; text: string }
	| { type: "thinking_redacted" }
	| {
			type: "tool_call";
			id: string;
			tool_name: string;
			args_json: string;
	  };

export interface ToolCall {
	id: string;
	tool_name: string;
	args_json: string;
}

export interface ToolResultMessage {
	tool_call_id: string;
	tool_name: string;
	output: string;
	status: "Success" | "Error" | "Interrupted" | "Crashed" | string;
}

export type TranscriptItem =
	| { type: "turn_started"; turn_id: number }
	| { type: "user_message"; content: ContentBlock[] }
	| { type: "assistant_message"; items: AssistantItem[] }
	| { type: "tool_call_started"; turn_id: number; tool_call: ToolCall }
	| { type: "tool_result"; tool_call_id: string; tool_name: string; output: string; status: string }
	| { type: "turn_finished"; turn_id: number; outcome: "Graceful" | "Interrupted" | "Crashed" | string }
	| { type: "injected"; kind: string; content: string; metadata: Record<string, string> };

export interface TranscriptEntry {
	id: string;
	parent_id: string | null;
	timestamp_ms: number;
	item: TranscriptItem;
}

export interface HistoryTree {
	session_id: string;
	active_leaf_id: string | null;
	entries: TranscriptEntry[];
}

export interface ToolDefinition {
	name: string;
	description: string;
	input_schema: unknown;
}

export type NoticeTone = "info" | "success" | "error";

export interface Notice {
	id: string;
	tone: NoticeTone;
	text: string;
}
