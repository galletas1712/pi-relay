export type EntryScope = "full_tree" | "active_branch";

export const queryKeys = {
	systemPromptRoot: ["system-prompt"] as const,
	systemPrompt: (sessionId: string) => ["system-prompt", sessionId] as const,
	projects: ["projects"] as const,
	tools: (provider: string) => ["tools", provider] as const,
	sessions: (projectId: string | null) => ["sessions", projectId] as const,
	subagents: (sessionId: string | null) => ["subagents", sessionId] as const,
	agentTranscriptPreview: (rootSessionId: string | null, sessionId: string | null) =>
		["agent-transcript-preview", rootSessionId, sessionId] as const,
	session: (sessionId: string, scope: EntryScope = "full_tree") => ["session", sessionId, scope] as const,
	historyTree: (sessionId: string, lastEventId: number) => ["history-tree", sessionId, lastEventId] as const,
};
