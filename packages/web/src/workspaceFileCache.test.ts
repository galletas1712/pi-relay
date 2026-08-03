import { describe, expect, it } from "vitest";
import {
	FILE_CACHE_BUDGET_BYTES,
	WorkspaceFileCache,
	type CachedWorkspaceFile,
} from "./workspaceFileCache.ts";

function file(
	sessionId: string,
	path: string,
	size: number,
): CachedWorkspaceFile {
	return {
		sessionId,
		path,
		bytes: new Uint8Array(size),
		totalSize: size,
		mtimeMs: null,
	};
}

describe("WorkspaceFileCache", () => {
	it("pins the open file and evicts oversized entries once unpinned", () => {
		const cache = new WorkspaceFileCache();
		const huge = FILE_CACHE_BUDGET_BYTES + 1;
		cache.set(file("s1", "huge.bin", huge), true);
		expect(cache.get("s1", "huge.bin")?.bytes.byteLength).toBe(huge);

		cache.unpin("s1", "huge.bin");
		expect(cache.get("s1", "huge.bin")).toBeNull();
	});

	it("evicts LRU unpinned entries beyond the count cap", () => {
		const cache = new WorkspaceFileCache();
		for (let i = 0; i < 18; i += 1) {
			cache.set(file("s1", `f${i}.txt`, 10), false);
		}
		expect(cache.size()).toBe(16);
		expect(cache.get("s1", "f0.txt")).toBeNull();
		expect(cache.get("s1", "f1.txt")).toBeNull();
		expect(cache.get("s1", "f17.txt")).not.toBeNull();
	});

	it("clears one session without touching another", () => {
		const cache = new WorkspaceFileCache();
		cache.set(file("s1", "a.txt", 4), false);
		cache.set(file("s2", "a.txt", 4), false);
		cache.clearSession("s1");
		expect(cache.get("s1", "a.txt")).toBeNull();
		expect(cache.get("s2", "a.txt")).not.toBeNull();
	});

	it("keeps a pinned file while evicting other unpinned pressure", () => {
		const cache = new WorkspaceFileCache();
		cache.set(file("s1", "open.txt", 100), true);
		for (let i = 0; i < 20; i += 1) {
			cache.set(file("s1", `other-${i}.txt`, 10), false);
		}
		expect(cache.get("s1", "open.txt")).not.toBeNull();
		expect(cache.size()).toBe(17); // 16 unpinned + 1 pinned
	});
});
