const UI_RESUME_STORAGE_KEY = "piRelayUiResume:v1";
const HOST_PROJECT_SESSION_KEY = "__host__";

export type UiResumeStorage = Pick<Storage, "getItem" | "setItem" | "removeItem">;

export interface UiResumeState {
	selectedProjectId: string | null;
	selectedSessionIdByProject: Record<string, string>;
	selectedSubagentIdByParentSession: Record<string, string>;
	/** Per root session: last Agents vs Files center surface. */
	centerModeByRootSession: Record<string, "chat" | "files">;
	/** Per conversation session: last opened file path (cwd-relative). */
	lastFileBySession: Record<string, string>;
	activeRootSessionId: string | null | undefined;
	updatedAt: number;
}

export interface UiSelection {
	projectId: string | null;
	sessionId: string | null;
}

const EMPTY_STATE: UiResumeState = {
	selectedProjectId: null,
	selectedSessionIdByProject: {},
	selectedSubagentIdByParentSession: {},
	centerModeByRootSession: {},
	lastFileBySession: {},
	activeRootSessionId: undefined,
	updatedAt: 0,
};

export function loadUiSelection(storage = browserStorage()): UiSelection {
	const state = readUiResumeState(storage);
	return {
		projectId: state.selectedProjectId,
		sessionId:
			state.activeRootSessionId === undefined
				? selectedSessionForProject(state.selectedProjectId, storage, state)
				: state.activeRootSessionId,
	};
}

export function selectedSessionForProject(
	projectId: string | null,
	storage = browserStorage(),
	state = readUiResumeState(storage),
): string | null {
	const key = projectSessionKey(projectId);
	return Object.hasOwn(state.selectedSessionIdByProject, key)
		? state.selectedSessionIdByProject[key] ?? null
		: null;
}

export function rememberUiSelection(projectId: string | null, sessionId: string | null, storage = browserStorage()): void {
	const state = readUiResumeState(storage);
	state.selectedProjectId = projectId;
	state.activeRootSessionId = sessionId;
	writeProjectSession(state, projectId, sessionId);
	writeUiResumeState(state, storage);
}

export function rememberActiveUiSelection(
	projectId: string | null,
	sessionId: string | null,
	storage = browserStorage(),
): void {
	const state = readUiResumeState(storage);
	state.selectedProjectId = projectId;
	state.activeRootSessionId = sessionId;
	writeUiResumeState(state, storage);
}

export function rememberSelectedSession(projectId: string | null, sessionId: string | null, storage = browserStorage()): void {
	const state = readUiResumeState(storage);
	const previousSessionId = selectedSessionForProject(projectId, storage, state);
	writeProjectSession(state, projectId, sessionId);
	if (
		sessionId === null &&
		state.selectedProjectId === projectId &&
		state.activeRootSessionId === previousSessionId
	) {
		state.activeRootSessionId = null;
	}
	writeUiResumeState(state, storage);
}

export function selectedSubagentForSession(
	parentSessionId: string,
	storage = browserStorage(),
	state = readUiResumeState(storage),
): string | null {
	return Object.hasOwn(state.selectedSubagentIdByParentSession, parentSessionId)
		? state.selectedSubagentIdByParentSession[parentSessionId] ?? null
		: null;
}

export function rememberSelectedSubagent(
	parentSessionId: string,
	subagentSessionId: string | null,
	storage = browserStorage(),
): void {
	const state = readUiResumeState(storage);
	if (subagentSessionId) state.selectedSubagentIdByParentSession[parentSessionId] = subagentSessionId;
	else delete state.selectedSubagentIdByParentSession[parentSessionId];
	writeUiResumeState(state, storage);
}

export function centerModeForRootSession(
	rootSessionId: string,
	storage = browserStorage(),
	state = readUiResumeState(storage),
): "chat" | "files" {
	return state.centerModeByRootSession[rootSessionId] === "files" ? "files" : "chat";
}

export function rememberCenterModeForRootSession(
	rootSessionId: string,
	mode: "chat" | "files",
	storage = browserStorage(),
): void {
	const state = readUiResumeState(storage);
	if (mode === "chat") delete state.centerModeByRootSession[rootSessionId];
	else state.centerModeByRootSession[rootSessionId] = "files";
	writeUiResumeState(state, storage);
}

export function lastFileForSession(
	sessionId: string,
	storage = browserStorage(),
	state = readUiResumeState(storage),
): string | null {
	return Object.hasOwn(state.lastFileBySession, sessionId)
		? state.lastFileBySession[sessionId] ?? null
		: null;
}

export function rememberLastFileForSession(
	sessionId: string,
	path: string | null,
	storage = browserStorage(),
): void {
	const state = readUiResumeState(storage);
	if (path) state.lastFileBySession[sessionId] = path;
	else delete state.lastFileBySession[sessionId];
	writeUiResumeState(state, storage);
}

export function forgetDeletedSessions(
	sessionIds: Iterable<string>,
	storage = browserStorage(),
): void {
	const deleted = new Set(sessionIds);
	if (!deleted.size) return;
	const state = readUiResumeState(storage);
	for (const [projectId, sessionId] of Object.entries(state.selectedSessionIdByProject)) {
		if (deleted.has(sessionId)) delete state.selectedSessionIdByProject[projectId];
	}
	for (const [parentSessionId, subagentSessionId] of Object.entries(
		state.selectedSubagentIdByParentSession,
	)) {
		if (deleted.has(parentSessionId) || deleted.has(subagentSessionId)) {
			delete state.selectedSubagentIdByParentSession[parentSessionId];
		}
	}
	for (const sessionId of Object.keys(state.centerModeByRootSession)) {
		if (deleted.has(sessionId)) delete state.centerModeByRootSession[sessionId];
	}
	for (const sessionId of Object.keys(state.lastFileBySession)) {
		if (deleted.has(sessionId)) delete state.lastFileBySession[sessionId];
	}
	if (state.activeRootSessionId && deleted.has(state.activeRootSessionId)) {
		state.activeRootSessionId = null;
	}
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
		const activeRootSessionId =
			state.activeRootSessionId === undefined
				? selectedSessionForProject(state.selectedProjectId, storage, state)
				: state.activeRootSessionId;
		const next: UiResumeState = {
			selectedProjectId: state.selectedProjectId,
			selectedSessionIdByProject: { ...state.selectedSessionIdByProject },
			selectedSubagentIdByParentSession: { ...state.selectedSubagentIdByParentSession },
			centerModeByRootSession: { ...state.centerModeByRootSession },
			lastFileBySession: { ...state.lastFileBySession },
			activeRootSessionId,
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
	const selectedSessionIdByProject = emptyIdMap();
	copyNormalizedIdMap(value.selectedSessionIdByProject, selectedSessionIdByProject);
	const selectedSubagentIdByParentSession = emptyIdMap();
	copyNormalizedIdMap(value.selectedSubagentIdByParentSession, selectedSubagentIdByParentSession);
	const centerModeByRootSession = emptyCenterModeMap();
	copyNormalizedCenterModeMap(value.centerModeByRootSession, centerModeByRootSession);
	const lastFileBySession = emptyIdMap();
	copyNormalizedIdMap(value.lastFileBySession, lastFileBySession);

	const legacySelectedSessionId = normalizeId(value.selectedSessionId);
	if (
		selectedProjectId &&
		legacySelectedSessionId &&
		!Object.hasOwn(selectedSessionIdByProject, selectedProjectId)
	) {
		selectedSessionIdByProject[selectedProjectId] = legacySelectedSessionId;
	}
	const activeRootSessionId = Object.hasOwn(value, "activeRootSessionId")
		? normalizeId(value.activeRootSessionId)
		: Object.hasOwn(value, "selectedSessionId")
			? legacySelectedSessionId
			: undefined;

	return {
		selectedProjectId,
		selectedSessionIdByProject,
		selectedSubagentIdByParentSession,
		centerModeByRootSession,
		lastFileBySession,
		activeRootSessionId,
		updatedAt: typeof value.updatedAt === "number" && Number.isFinite(value.updatedAt) ? value.updatedAt : 0,
	};
}

function copyNormalizedIdMap(value: unknown, target: Record<string, string>): void {
	if (!isRecord(value)) return;
	for (const [scopeId, selectedId] of Object.entries(value)) {
		const normalizedSelectedId = normalizeId(selectedId);
		if (scopeId && normalizedSelectedId) target[scopeId] = normalizedSelectedId;
	}
}

function copyNormalizedCenterModeMap(
	value: unknown,
	target: Record<string, "chat" | "files">,
): void {
	if (!isRecord(value)) return;
	for (const [sessionId, mode] of Object.entries(value)) {
		if (!sessionId) continue;
		if (mode === "files" || mode === "chat") target[sessionId] = mode;
	}
}

function normalizeId(value: unknown): string | null {
	return typeof value === "string" && value.trim() ? value : null;
}

function emptyIdMap(): Record<string, string> {
	return Object.create(null) as Record<string, string>;
}

function emptyCenterModeMap(): Record<string, "chat" | "files"> {
	return Object.create(null) as Record<string, "chat" | "files">;
}

function cloneEmptyState(): UiResumeState {
	return {
		selectedProjectId: EMPTY_STATE.selectedProjectId,
		selectedSessionIdByProject: emptyIdMap(),
		selectedSubagentIdByParentSession: emptyIdMap(),
		centerModeByRootSession: emptyCenterModeMap(),
		lastFileBySession: emptyIdMap(),
		activeRootSessionId: EMPTY_STATE.activeRootSessionId,
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
