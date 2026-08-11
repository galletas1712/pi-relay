// Backend profile resolution. The legacy data layer stays the default; the
// bridge layer activates via build-time env (VITE_BACKEND=bridge) or a runtime
// override (?backend=bridge persists to localStorage so reloads keep it;
// ?backend=legacy switches back). Resolution order: query > localStorage > env
// > legacy.
export type BackendProfile = "legacy" | "bridge";

const STORAGE_KEY = "pi-relay:backend";

export function resolveBackendProfile(
	search: string = typeof window === "undefined" ? "" : window.location.search,
	storage: Pick<Storage, "getItem" | "setItem"> | null =
		typeof window === "undefined" ? null : window.localStorage,
	envBackend: string | undefined = import.meta.env?.VITE_BACKEND,
): BackendProfile {
	const query = new URLSearchParams(search).get("backend");
	if (query === "bridge" || query === "legacy") {
		try {
			storage?.setItem(STORAGE_KEY, query);
		} catch {
			// storage may be unavailable; the query value still wins for this load
		}
		return query;
	}
	try {
		const stored = storage?.getItem(STORAGE_KEY);
		if (stored === "bridge" || stored === "legacy") return stored;
	} catch {
		// fall through to env
	}
	return envBackend === "bridge" ? "bridge" : "legacy";
}
