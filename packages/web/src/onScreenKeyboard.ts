const COARSE_POINTER_QUERY = "(pointer: coarse)";

/**
 * True when the primary pointer is coarse (phones/tablets), which is the case
 * where focusing an input pops a software keyboard over the transcript.
 */
export function usesOnScreenKeyboard(): boolean {
	if (typeof window === "undefined" || typeof window.matchMedia !== "function") return false;
	return window.matchMedia(COARSE_POINTER_QUERY).matches;
}
