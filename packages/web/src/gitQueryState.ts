export const DEFAULT_GIT_HISTORY_LIMIT = 12;
export const MAX_GIT_HISTORY_LIMIT = 100;

export interface GitHistoryState {
	sessionId: string | null;
	limit: number;
}

export function gitHistoryForSession(
	state: GitHistoryState,
	sessionId: string | null,
): GitHistoryState {
	return state.sessionId === sessionId
		? state
		: { sessionId, limit: DEFAULT_GIT_HISTORY_LIMIT };
}

export function expandGitHistory(
	state: GitHistoryState,
	sessionId: string | null,
): GitHistoryState {
	const current = gitHistoryForSession(state, sessionId);
	return {
		sessionId,
		limit: current.limit < 50 ? 50 : MAX_GIT_HISTORY_LIMIT,
	};
}

