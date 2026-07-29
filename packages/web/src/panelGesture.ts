import type { PanelMode } from "./panelLayout.ts";

export const PANEL_GESTURE_EDGE_GUARD_PX = 32;
export const PANEL_GESTURE_INTENT_THRESHOLD_PX = 10;
export const PANEL_GESTURE_SWIPE_THRESHOLD_PX = 48;
const PANEL_GESTURE_DIRECTION_LOCK_MARGIN_PX = 12;

export type PanelGestureAction =
	| "open-sidebar"
	| "open-inspector"
	| "close-sidebar"
	| "close-inspector";

export interface PanelGesturePanelState {
	sidebarOpen: boolean;
	rightOpen: boolean;
}

export interface PanelGesturePointer {
	pointerId: number;
	pointerType: string;
	isPrimary: boolean;
	clientX: number;
	clientY: number;
	target?: EventTarget | null;
}

export interface PanelGestureMoveResult {
	/**
	 * The caller should prevent the browser's default gesture only after the
	 * full swipe threshold is reached with a clear horizontal direction.
	 */
	preventDefault: boolean;
}

export interface PanelGestureControllerOptions {
	edgeGuardPx?: number;
	intentThresholdPx?: number;
	swipeThresholdPx?: number;
	viewportWidth?: () => number;
}

interface ActiveGesture {
	pointerId: number;
	startX: number;
	startY: number;
	horizontalIntent: boolean;
}

/**
 * Recognizes center-started compact-panel swipes without owning any DOM
 * listeners or panel state. A caller must pass the current mode and panel
 * state to start/end, so a gesture cannot apply an action to a stale layout.
 */
export class PanelGestureController {
	private readonly edgeGuardPx: number;
	private readonly intentThresholdPx: number;
	private readonly swipeThresholdPx: number;
	private readonly viewportWidth: () => number;
	private active: ActiveGesture | null = null;

	constructor(options: PanelGestureControllerOptions = {}) {
		this.edgeGuardPx = options.edgeGuardPx ?? PANEL_GESTURE_EDGE_GUARD_PX;
		this.intentThresholdPx =
			options.intentThresholdPx ?? PANEL_GESTURE_INTENT_THRESHOLD_PX;
		this.swipeThresholdPx =
			options.swipeThresholdPx ?? PANEL_GESTURE_SWIPE_THRESHOLD_PX;
		this.viewportWidth =
			options.viewportWidth ??
			(() =>
				typeof window === "undefined"
					? Number.POSITIVE_INFINITY
					: window.innerWidth);
	}

	get activePointerId(): number | null {
		return this.active?.pointerId ?? null;
	}

	start(pointer: PanelGesturePointer, mode: PanelMode): boolean {
		if (mode !== "compact" || this.active !== null) return false;
		if (!isEligiblePointer(pointer)) return false;
		if (isInteractiveTarget(pointer.target)) return false;
		if (isExcludedPanelTarget(pointer.target)) return false;
		if (isViewportEdgeStart(pointer.clientX, this.viewportWidth(), this.edgeGuardPx)) return false;

		this.active = {
			pointerId: pointer.pointerId,
			startX: pointer.clientX,
			startY: pointer.clientY,
			horizontalIntent: false,
		};
		return true;
	}

	move(pointer: PanelGesturePointer): PanelGestureMoveResult {
		const active = this.active;
		if (!active || active.pointerId !== pointer.pointerId) {
			return { preventDefault: false };
		}

		const deltaX = pointer.clientX - active.startX;
		const deltaY = pointer.clientY - active.startY;
		const horizontalDistance = Math.abs(deltaX);
		const verticalDistance = Math.abs(deltaY);
		if (!active.horizontalIntent) {
			if (verticalDistance >= horizontalDistance && verticalDistance >= this.intentThresholdPx) {
				this.active = null;
				return { preventDefault: false };
			}
			if (
				horizontalDistance >= this.intentThresholdPx &&
				horizontalDistance >= verticalDistance + PANEL_GESTURE_DIRECTION_LOCK_MARGIN_PX
			) {
				active.horizontalIntent = true;
			}
		}

		if (
			active.horizontalIntent &&
			horizontalDistance >= this.swipeThresholdPx &&
			horizontalDistance >= verticalDistance + PANEL_GESTURE_DIRECTION_LOCK_MARGIN_PX
		) {
			return { preventDefault: true };
		}
		return { preventDefault: false };
	}

	end(
		pointer: PanelGesturePointer,
		mode: PanelMode,
		state: PanelGesturePanelState,
	): PanelGestureAction | null {
		const active = this.active;
		if (!active || active.pointerId !== pointer.pointerId) return null;
		this.active = null;

		if (mode !== "compact" || !active.horizontalIntent) return null;
		const deltaX = pointer.clientX - active.startX;
		const deltaY = pointer.clientY - active.startY;
		if (
			Math.abs(deltaX) < this.swipeThresholdPx ||
			Math.abs(deltaX) < Math.abs(deltaY) + PANEL_GESTURE_DIRECTION_LOCK_MARGIN_PX
		) {
			return null;
		}

		if (deltaX > 0) {
			if (state.rightOpen) return "close-inspector";
			if (!state.sidebarOpen) return "open-sidebar";
			return null;
		}
		if (state.sidebarOpen) return "close-sidebar";
		if (!state.rightOpen) return "open-inspector";
		return null;
	}

	cancel(pointerId?: number): boolean {
		if (pointerId !== undefined && this.active?.pointerId !== pointerId) return false;
		const wasActive = this.active !== null;
		this.active = null;
		return wasActive;
	}

	reset(): void {
		this.active = null;
	}
}

function isEligiblePointer(pointer: PanelGesturePointer): boolean {
	return pointer.pointerType === "touch" && pointer.isPrimary;
}

function isViewportEdgeStart(clientX: number, viewportWidth: number, edgeGuardPx: number): boolean {
	if (!Number.isFinite(clientX) || !Number.isFinite(viewportWidth) || viewportWidth <= 0) {
		return true;
	}
	return clientX < edgeGuardPx || clientX > viewportWidth - edgeGuardPx;
}

function isInteractiveTarget(target: EventTarget | null | undefined): boolean {
	let element = elementFromTarget(target);
	while (element) {
		const tagName = element.tagName.toLowerCase();
		if (
			tagName === "button" ||
			tagName === "a" ||
			tagName === "input" ||
			tagName === "textarea" ||
			tagName === "select" ||
			tagName === "label"
		) {
			return true;
		}
		if (element.hasAttribute("data-panel-gesture-ignore")) return true;
		const contentEditable = element.getAttribute("contenteditable");
		if (contentEditable !== null && contentEditable.toLowerCase() !== "false") return true;
		if (
			typeof HTMLElement !== "undefined" &&
			element instanceof HTMLElement &&
			typeof element.contentEditable === "string" &&
			element.contentEditable.toLowerCase() === "true"
		) {
			return true;
		}
		element = element.parentElement;
	}
	return false;
}

function isExcludedPanelTarget(target: EventTarget | null | undefined): boolean {
	const element = elementFromTarget(target);
	return !!element?.closest(
		".mobile-topbar, .sidebar-resize-handle, " +
		".assistant-markdown pre, .assistant-markdown table, " +
		".tool-run-items.scrollable, .tool-run-group pre, .tool-card pre",
	);
}

function elementFromTarget(target: EventTarget | null | undefined): Element | null {
	if (!target) return null;
	if (typeof (target as Element).closest === "function") return target as Element;
	if (typeof Node !== "undefined" && target instanceof Node) return target.parentElement;
	return null;
}
