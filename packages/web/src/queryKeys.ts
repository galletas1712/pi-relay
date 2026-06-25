export type EntryScope = "full_tree" | "active_branch";

export const queryKeys = {
	systemPromptRoot: ["system-prompt"] as const,
	systemPrompt: (sessionId: string) => ["system-prompt", sessionId] as const,
	projects: ["projects"] as const,
	tools: (provider: string, sessionId: string | null = null) => ["tools", provider, sessionId] as const,
	sessions: (projectId: string | null) => ["sessions", projectId] as const,
	delegations: (parentSessionId: string | null, limit?: number) => ["delegations", parentSessionId, limit ?? null] as const,
	session: (sessionId: string, scope: EntryScope = "full_tree") => ["session", sessionId, scope] as const,
	historyTree: (sessionId: string, lastEventId: number) => ["history-tree", sessionId, lastEventId] as const,
};
