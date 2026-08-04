import { useLayoutEffect, useRef } from "react";

/**
 * Preserve a scroller's scrollTop across parked ↔ active transitions.
 * Capture runs during render (DOM still has pre-park layout); restore runs in
 * layout + rAF so later resize/sticky handlers cannot clobber it.
 * Does nothing on initial mount — only when leaving a parked state.
 */
export function useParkedScrollPreservation(
	getScroller: () => HTMLElement | null,
	parked: boolean,
): void {
	const savedScrollTopRef = useRef(0);
	const prevParkedRef = useRef(parked);
	const hasBeenParkedRef = useRef(false);
	const getScrollerRef = useRef(getScroller);
	getScrollerRef.current = getScroller;

	if (prevParkedRef.current !== parked) {
		if (parked) {
			const scroller = getScrollerRef.current();
			if (scroller) savedScrollTopRef.current = scroller.scrollTop;
			hasBeenParkedRef.current = true;
		}
		prevParkedRef.current = parked;
	}

	useLayoutEffect(() => {
		if (parked || !hasBeenParkedRef.current) return;
		const scroller = getScrollerRef.current();
		if (!scroller) return;
		const top = savedScrollTopRef.current;
		scroller.scrollTop = top;
		const raf = requestAnimationFrame(() => {
			scroller.scrollTop = top;
		});
		return () => cancelAnimationFrame(raf);
	}, [parked]);
}
