/** In-memory workspace file cache with pinned-open + LRU eviction. */

export const FILE_CACHE_BUDGET_BYTES = 8 * 1024 * 1024 * 1024;
export const FILE_CACHE_MAX_UNPINNED = 16;

export type CachedWorkspaceFile = {
	sessionId: string;
	path: string;
	bytes: Uint8Array;
	totalSize: number;
	mtimeMs: number | null;
};

type CacheEntry = {
	file: CachedWorkspaceFile;
	pinned: boolean;
	lastUsed: number;
};

function cacheKey(sessionId: string, path: string): string {
	return `${sessionId}\0${path}`;
}

export class WorkspaceFileCache {
	private readonly entries = new Map<string, CacheEntry>();
	private clock = 0;

	get(sessionId: string, path: string): CachedWorkspaceFile | null {
		const entry = this.entries.get(cacheKey(sessionId, path));
		if (!entry) return null;
		entry.lastUsed = ++this.clock;
		return entry.file;
	}

	set(file: CachedWorkspaceFile, pinned: boolean): void {
		const key = cacheKey(file.sessionId, file.path);
		this.entries.set(key, {
			file,
			pinned,
			lastUsed: ++this.clock,
		});
		this.evict();
	}

	pin(sessionId: string, path: string): void {
		const entry = this.entries.get(cacheKey(sessionId, path));
		if (!entry) return;
		entry.pinned = true;
		entry.lastUsed = ++this.clock;
	}

	unpin(sessionId: string, path: string): void {
		const entry = this.entries.get(cacheKey(sessionId, path));
		if (!entry) return;
		entry.pinned = false;
		entry.lastUsed = ++this.clock;
		this.evict();
	}

	invalidate(sessionId: string, path: string): void {
		this.entries.delete(cacheKey(sessionId, path));
	}

	clearSession(sessionId: string): void {
		for (const key of [...this.entries.keys()]) {
			if (key.startsWith(`${sessionId}\0`)) this.entries.delete(key);
		}
	}

	clear(): void {
		this.entries.clear();
	}

	/** Test/debug helpers. */
	size(): number {
		return this.entries.size;
	}

	unpinnedBytes(): number {
		let total = 0;
		for (const entry of this.entries.values()) {
			if (!entry.pinned) total += entry.file.bytes.byteLength;
		}
		return total;
	}

	private evict(): void {
		for (;;) {
			const unpinned = [...this.entries.entries()].filter(([, entry]) => !entry.pinned);
			const unpinnedBytes = unpinned.reduce(
				(sum, [, entry]) => sum + entry.file.bytes.byteLength,
				0,
			);
			const overBudget =
				unpinnedBytes > FILE_CACHE_BUDGET_BYTES || unpinned.length > FILE_CACHE_MAX_UNPINNED;
			if (!overBudget) return;
			unpinned.sort(([, a], [, b]) => a.lastUsed - b.lastUsed);
			const victim = unpinned[0];
			if (!victim) return;
			this.entries.delete(victim[0]);
		}
	}
}

/** Process-wide cache shared by the Files pane. */
export const workspaceFileCache = new WorkspaceFileCache();
