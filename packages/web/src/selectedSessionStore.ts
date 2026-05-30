import { useCallback, useRef, useState, type RefObject } from "react";
import {
	emptySelectedSessionCache,
	type SelectedSessionCache,
} from "./selectedSessionCache.ts";

export type SelectedSessionCacheUpdater = (current: SelectedSessionCache) => SelectedSessionCache;

export interface SelectedSessionStore {
	cache: SelectedSessionCache;
	cacheRef: RefObject<SelectedSessionCache>;
	replace: (cache: SelectedSessionCache) => SelectedSessionCache;
	reset: (sessionId: string | null) => SelectedSessionCache;
	update: (updater: SelectedSessionCacheUpdater) => SelectedSessionCache;
}

export function useSelectedSessionStore(initialSessionId: string | null): SelectedSessionStore {
	const [cache, setCache] = useState<SelectedSessionCache>(() => emptySelectedSessionCache(initialSessionId));
	const cacheRef = useRef<SelectedSessionCache>(cache);

	const replace = useCallback((next: SelectedSessionCache) => {
		cacheRef.current = next;
		setCache(next);
		return next;
	}, []);

	const reset = useCallback(
		(sessionId: string | null) => replace(emptySelectedSessionCache(sessionId)),
		[replace],
	);

	const update = useCallback(
		(updater: SelectedSessionCacheUpdater) => replace(updater(cacheRef.current)),
		[replace],
	);

	return { cache, cacheRef, replace, reset, update };
}
