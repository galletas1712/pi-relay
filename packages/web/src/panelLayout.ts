export type PanelMode = "compact" | "medium" | "wide";

export const MEDIUM_PANEL_QUERY = "(min-width: 900px)";
export const WIDE_PANEL_QUERY = "(min-width: 1280px)";
export const SIDEBAR_WIDTH_STORAGE_KEY = "piRelaySidebarWidth:v1";
export const DEFAULT_SIDEBAR_WIDTH = 320;
export const MIN_SIDEBAR_WIDTH = 240;
export const MAX_SIDEBAR_WIDTH = 480;
export const SIDEBAR_KEYBOARD_STEP = 16;

export const FILE_PANE_WIDTH_STORAGE_KEY = "piRelayFilePaneWidth:v1";
export const DEFAULT_FILE_PANE_WIDTH = 440;
export const MIN_FILE_PANE_WIDTH = 360;
/** Matches the transcript/composer centered column (`width: min(100%, 900px)`). */
export const CHAT_COLUMN_MAX_WIDTH = 900;
/** File pane can grow as wide as the chat column when split. */
export const MAX_FILE_PANE_WIDTH = CHAT_COLUMN_MAX_WIDTH;
/**
 * Split only when the center strip can host a full-width chat column and an
 * equally wide file column at once (sidebar/inspector excluded).
 */
export const FILE_SPLIT_MIN_CENTER_PX = CHAT_COLUMN_MAX_WIDTH * 2;

/** Whether the app-shell center (chat + file) is wide enough to split. */
export function canSplitFilePane(centerWidthPx: number): boolean {
	return centerWidthPx >= FILE_SPLIT_MIN_CENTER_PX;
}

export function clampSidebarWidth(width: number): number {
	return Math.min(MAX_SIDEBAR_WIDTH, Math.max(MIN_SIDEBAR_WIDTH, Math.round(width)));
}

export function loadSidebarWidth(): number {
	if (typeof window === "undefined") return DEFAULT_SIDEBAR_WIDTH;
	try {
		const stored = Number(window.localStorage.getItem(SIDEBAR_WIDTH_STORAGE_KEY));
		return Number.isFinite(stored) && stored > 0
			? clampSidebarWidth(stored)
			: DEFAULT_SIDEBAR_WIDTH;
	} catch {
		return DEFAULT_SIDEBAR_WIDTH;
	}
}

export function saveSidebarWidth(width: number): void {
	try {
		window.localStorage.setItem(SIDEBAR_WIDTH_STORAGE_KEY, String(clampSidebarWidth(width)));
	} catch {
		// localStorage persistence is best-effort.
	}
}

export function clampFilePaneWidth(width: number): number {
	return Math.min(MAX_FILE_PANE_WIDTH, Math.max(MIN_FILE_PANE_WIDTH, Math.round(width)));
}

export function loadFilePaneWidth(): number {
	if (typeof window === "undefined") return DEFAULT_FILE_PANE_WIDTH;
	try {
		const stored = Number(window.localStorage.getItem(FILE_PANE_WIDTH_STORAGE_KEY));
		return Number.isFinite(stored) && stored > 0
			? clampFilePaneWidth(stored)
			: DEFAULT_FILE_PANE_WIDTH;
	} catch {
		return DEFAULT_FILE_PANE_WIDTH;
	}
}

export function saveFilePaneWidth(width: number): void {
	try {
		window.localStorage.setItem(FILE_PANE_WIDTH_STORAGE_KEY, String(clampFilePaneWidth(width)));
	} catch {
		// localStorage persistence is best-effort.
	}
}

export function panelModeForViewport(): PanelMode {
	if (typeof window === "undefined" || typeof window.matchMedia !== "function") return "wide";
	if (window.matchMedia(WIDE_PANEL_QUERY).matches) return "wide";
	if (window.matchMedia(MEDIUM_PANEL_QUERY).matches) return "medium";
	return "compact";
}

export function defaultPanelState(mode: PanelMode): { sidebarOpen: boolean; rightOpen: boolean } {
	return {
		sidebarOpen: mode === "wide",
		rightOpen: mode !== "compact",
	};
}
