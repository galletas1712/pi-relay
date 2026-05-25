const UI_RESUME_STORAGE_KEY = "piRelayUiResume:v1";
const HOST_PROJECT_SESSION_KEY = "__host__";

export type UiResumeStorage = Pick<Storage, "getItem" | "setItem" | "removeItem">;

export interface UiResumeState {
	selectedProjectId: string | null;
	selectedSessionIdByProject: Record<string, string>;
	updatedAt: number;
}

export interface UiSelection {
	projectId: string | null;
	sessionId: string | null;
}

const EMPTY_STATE: UiResumeState = {
	selectedProjectId: null,
	selectedSessionIdByProject: {},
	updatedAt: 0,
};

export function loadUiSelection(storage = browserStorage()): UiSelection {
	const state = readUiResumeState(storage);
	return {
		projectId: state.selectedProjectId,
		sessionId: selectedSessionForProject(state.selectedProjectId, storage, state),
	};
}

export function selectedSessionForProject(
	projectId: string | null,
	storage = browserStorage(),
	state = readUiResumeState(storage),
): string | null {
	return state.selectedSessionIdByProject[projectSessionKey(projectId)] ?? null;
}

export function rememberUiSelection(projectId: string | null, sessionId: string | null, storage = browserStorage()): void {
	const state = readUiResumeState(storage);
	state.selectedProjectId = projectId;
	writeProjectSession(state, projectId, sessionId);
	writeUiResumeState(state, storage);
}

export function rememberSelectedSession(projectId: string | null, sessionId: string | null, storage = browserStorage()): void {
	const state = readUiResumeState(storage);
	writeProjectSession(state, projectId, sessionId);
	writeUiResumeState(state, storage);
}

export function readUiResumeState(storage = browserStorage()): UiResumeState {
	if (!storage) return cloneEmptyState();
	try {
		const raw = storage.getItem(UI_RESUME_STORAGE_KEY);
		if (!raw) return cloneEmptyState();
		return normalizeState(JSON.parse(raw) as unknown);
	} catch {
		return cloneEmptyState();
	}
}

function writeUiResumeState(state: UiResumeState, storage: UiResumeStorage | null): void {
	if (!storage) return;
	try {
		const next: UiResumeState = {
			selectedProjectId: state.selectedProjectId,
			selectedSessionIdByProject: { ...state.selectedSessionIdByProject },
			updatedAt: Date.now(),
		};
		storage.setItem(UI_RESUME_STORAGE_KEY, JSON.stringify(next));
	} catch {
		// localStorage can be unavailable or full; selection persistence is best-effort.
	}
}

function writeProjectSession(state: UiResumeState, projectId: string | null, sessionId: string | null): void {
	const key = projectSessionKey(projectId);
	if (sessionId) state.selectedSessionIdByProject[key] = sessionId;
	else delete state.selectedSessionIdByProject[key];
}

function projectSessionKey(projectId: string | null): string {
	return projectId ?? HOST_PROJECT_SESSION_KEY;
}

function normalizeState(value: unknown): UiResumeState {
	if (!isRecord(value)) return cloneEmptyState();
	const selectedProjectId = normalizeId(value.selectedProjectId);
	const selectedSessionIdByProject: Record<string, string> = {};
	const projectSessions = value.selectedSessionIdByProject;
	if (isRecord(projectSessions)) {
		for (const [projectId, sessionId] of Object.entries(projectSessions)) {
			const normalizedSessionId = normalizeId(sessionId);
			if (projectId && normalizedSessionId) selectedSessionIdByProject[projectId] = normalizedSessionId;
		}
	}

	const legacySelectedSessionId = normalizeId(value.selectedSessionId);
	if (selectedProjectId && legacySelectedSessionId && !selectedSessionIdByProject[selectedProjectId]) {
		selectedSessionIdByProject[selectedProjectId] = legacySelectedSessionId;
	}

	return {
		selectedProjectId,
		selectedSessionIdByProject,
		updatedAt: typeof value.updatedAt === "number" && Number.isFinite(value.updatedAt) ? value.updatedAt : 0,
	};
}

function normalizeId(value: unknown): string | null {
	return typeof value === "string" && value.trim() ? value : null;
}

function cloneEmptyState(): UiResumeState {
	return {
		selectedProjectId: EMPTY_STATE.selectedProjectId,
		selectedSessionIdByProject: {},
		updatedAt: EMPTY_STATE.updatedAt,
	};
}

function browserStorage(): UiResumeStorage | null {
	if (typeof window === "undefined") return null;
	try {
		return window.localStorage ?? null;
	} catch {
		return null;
	}
}

function isRecord(value: unknown): value is Record<string, unknown> {
	return typeof value === "object" && value !== null && !Array.isArray(value);
}

export { UI_RESUME_STORAGE_KEY };
