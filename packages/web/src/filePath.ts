/** Shared validation for cwd-relative browse paths (mirrors the runtime rules). */

export function validateBrowsePath(path: string): string | null {
	if (path === "") return "";
	if (path.includes("\0") || path.includes("\\")) return null;
	if (path.startsWith("/")) return null;
	if (path.includes("//") || path.endsWith("/")) return null;
	for (const ch of path) {
		if (ch < " " || ch === "\u007f") return null;
	}
	const parts = path.split("/");
	if (parts.length === 0 || parts.some((part) => !part || part === "." || part === "..")) {
		return null;
	}
	return parts.join("/");
}

export function joinBrowsePath(parent: string, name: string): string {
	return parent ? `${parent}/${name}` : name;
}

export function parentBrowsePath(path: string): string {
	const idx = path.lastIndexOf("/");
	return idx === -1 ? "" : path.slice(0, idx);
}

export function browsePathBasename(path: string): string {
	const idx = path.lastIndexOf("/");
	return idx === -1 ? path : path.slice(idx + 1);
}

export function readFileQuery(search: string = typeof window === "undefined" ? "" : window.location.search): string | null {
	const params = new URLSearchParams(search.startsWith("?") ? search.slice(1) : search);
	const raw = params.get("file");
	if (raw == null || raw === "") return null;
	return validateBrowsePath(raw);
}

/** Patch only the `file` query param via replaceState; leave path and other params alone. */
export function replaceFileQuery(path: string | null): void {
	if (typeof window === "undefined") return;
	const url = new URL(window.location.href);
	if (path == null || path === "") {
		url.searchParams.delete("file");
	} else {
		url.searchParams.set("file", path);
	}
	const next = `${url.pathname}${url.search}${url.hash}`;
	const current = `${window.location.pathname}${window.location.search}${window.location.hash}`;
	if (next === current) return;
	window.history.replaceState(window.history.state, "", next);
}
