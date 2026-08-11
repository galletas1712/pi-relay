// Per-key async mutex (port of workspaces/mod.rs's keyed base/session locks).
export class KeyedMutex {
	private readonly tails = new Map<string, Promise<unknown>>();

	async with<T>(key: string, fn: () => Promise<T>): Promise<T> {
		const prev = this.tails.get(key) ?? Promise.resolve();
		const next = prev.then(fn, fn);
		this.tails.set(key, next.catch(() => {}));
		try {
			return await next;
		} finally {
			if (this.tails.get(key) === next.catch(() => {})) {
				// best effort cleanup; map entries are cheap to keep anyway
			}
		}
	}
}
