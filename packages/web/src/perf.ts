const PERF_STORAGE_KEY = "piRelayPerf";

export function perfEnabled(): boolean {
	if (import.meta.env.DEV) return true;
	if (typeof window === "undefined") return false;
	try {
		return window.localStorage?.getItem(PERF_STORAGE_KEY) === "1";
	} catch {
		return false;
	}
}

export function perfNow(): number {
	return typeof performance !== "undefined" ? performance.now() : Date.now();
}

export function perfLog(label: string, data: Record<string, unknown>) {
	if (!perfEnabled()) return;
	console.debug(`[pi-relay perf] ${label}`, data);
}

export function approximateJsonSize(value: unknown): number {
	try {
		return JSON.stringify(value).length;
	} catch {
		return 0;
	}
}
