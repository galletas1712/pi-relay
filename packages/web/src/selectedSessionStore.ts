import { useCallback, useRef, useState, type RefObject } from "react";
import {
	emptySelectedSessionCache,
	type SelectedSessionCache,
} from "./selectedSessionCache.ts";

export type SelectedSessionCacheUpdater = (current: SelectedSessionCache) => SelectedSessionCache;

export interface SelectedSessionStore {
	cache: SelectedSessionCache;
	cacheRef: RefObject<SelectedSessionCache>;
	drop: (sessionId: string) => void;
	replace: (cache: SelectedSessionCache) => SelectedSessionCache;
	reset: (sessionId: string | null) => SelectedSessionCache;
	update: (updater: SelectedSessionCacheUpdater) => SelectedSessionCache;
}

export function useSelectedSessionStore(initialSessionId: string | null): SelectedSessionStore {
	const [cache, setCache] = useState<SelectedSessionCache>(() => emptySelectedSessionCache(initialSessionId));
	const cacheRef = useRef<SelectedSessionCache>(cache);
	const cachesRef = useRef<Map<string, SelectedSessionCache>>(new Map());

	const replace = useCallback((next: SelectedSessionCache) => {
		if (next.sessionId) cachesRef.current.set(next.sessionId, next);
		cacheRef.current = next;
		setCache(next);
		return next;
	}, []);

	const reset = useCallback(
		(sessionId: string | null) => replace((sessionId ? cachesRef.current.get(sessionId) : null) ?? emptySelectedSessionCache(sessionId)),
		[replace],
	);

	const drop = useCallback((sessionId: string) => {
		cachesRef.current.delete(sessionId);
		if (cacheRef.current.sessionId === sessionId) replace(emptySelectedSessionCache(null));
	}, [replace]);

	const update = useCallback(
		(updater: SelectedSessionCacheUpdater) => replace(updater(cacheRef.current)),
		[replace],
	);

	return { cache, cacheRef, drop, replace, reset, update };
}
