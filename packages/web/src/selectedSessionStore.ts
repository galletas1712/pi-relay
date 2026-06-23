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
	get: (sessionId: string) => SelectedSessionCache | null;
	replace: (cache: SelectedSessionCache) => SelectedSessionCache;
	reset: (sessionId: string | null) => SelectedSessionCache;
	update: (updater: SelectedSessionCacheUpdater) => SelectedSessionCache;
	warm: (sessionId: string, updater: SelectedSessionCacheUpdater) => SelectedSessionCache;
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

	const get = useCallback((sessionId: string) => {
		if (cacheRef.current.sessionId === sessionId) return cacheRef.current;
		return cachesRef.current.get(sessionId) ?? null;
	}, []);

	const update = useCallback(
		(updater: SelectedSessionCacheUpdater) => replace(updater(cacheRef.current)),
		[replace],
	);

	const warm = useCallback(
		(sessionId: string, updater: SelectedSessionCacheUpdater) => {
			const current =
				cacheRef.current.sessionId === sessionId
					? cacheRef.current
					: cachesRef.current.get(sessionId) ?? emptySelectedSessionCache(sessionId);
			const next = updater(current);
			if (next.sessionId) cachesRef.current.set(next.sessionId, next);
			if (cacheRef.current.sessionId === next.sessionId) {
				cacheRef.current = next;
				setCache(next);
			}
			return next;
		},
		[],
	);

	return { cache, cacheRef, drop, get, replace, reset, update, warm };
}
