export type EntryScope = "full_tree" | "active_branch";

export const queryKeys = {
	systemPromptRoot: ["system-prompt"] as const,
	systemPrompt: (sessionId: string) => ["system-prompt", sessionId] as const,
	projects: ["projects"] as const,
	tools: (provider: string) => ["tools", provider] as const,
	sessions: (projectId: string | null) => ["sessions", projectId] as const,
	stages: (parentSessionId: string | null) => ["stages", parentSessionId] as const,
	stage: (parentSessionId: string | null, stageId: string) => ["stage", parentSessionId, stageId] as const,
	session: (sessionId: string, scope: EntryScope = "full_tree") => ["session", sessionId, scope] as const,
	historyTree: (sessionId: string, lastEventId: number) => ["history-tree", sessionId, lastEventId] as const,
};
