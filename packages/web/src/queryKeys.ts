export type EntryScope = "full_tree" | "active_branch";

export const queryKeys = {
	systemPrompt: ["system-prompt"] as const,
	projects: ["projects"] as const,
	tools: (provider: string) => ["tools", provider] as const,
	sessions: (projectId: string | null) => ["sessions", projectId] as const,
	session: (sessionId: string, scope: EntryScope = "full_tree") => ["session", sessionId, scope] as const,
};
