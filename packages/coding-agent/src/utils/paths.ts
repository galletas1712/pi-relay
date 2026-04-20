/**
 * Returns true if the value is NOT a package source (npm:, git:, etc.)
 * or a URL protocol. Bare names and relative paths without ./ prefix
 * are considered local.
 */
export function isLocalPath(value: string): boolean {
	const trimmed = value.trim();
	// Known non-local prefixes
	if (
		trimmed.startsWith("npm:") ||
		trimmed.startsWith("git:") ||
		trimmed.startsWith("github:") ||
		trimmed.startsWith("http:") ||
		trimmed.startsWith("https:") ||
		trimmed.startsWith("ssh:")
	) {
		return false;
	}
	return true;
}

/**
 * Heuristic: `value` looks like a bare npm package name (possibly scoped),
 * not a filesystem path.
 *
 * A bare package name:
 *   - is a non-empty, non-whitespace string,
 *   - does NOT start with `.`, `/`, or `~`,
 *   - does NOT contain a Windows drive letter (e.g. `C:`),
 *   - does NOT contain any of the `npm:` / `git:` / `http(s):` / `ssh:` /
 *     `github:` / `file:` prefixes,
 *   - for scoped packages starts with `@` and contains exactly one `/`,
 *   - for unscoped packages contains no `/` at all.
 *
 * Used by the extension loader to decide whether a settings / CLI entry
 * should be resolved via Node module resolution vs. treated as a path.
 */
export function looksLikeBarePackageName(value: string): boolean {
	const trimmed = value.trim();
	if (!trimmed) return false;
	if (trimmed.startsWith(".") || trimmed.startsWith("/") || trimmed.startsWith("~")) {
		return false;
	}
	// Windows drive letter: a letter followed by `:` and a separator.
	if (/^[A-Za-z]:[\\/]/.test(trimmed)) return false;
	// URL-like scheme prefix (e.g. npm:/git:/http:/https:/ssh:/github:/file:).
	if (/^[A-Za-z][A-Za-z0-9+.-]*:/.test(trimmed)) return false;
	if (trimmed.startsWith("@")) {
		const parts = trimmed.split("/");
		if (parts.length !== 2) return false;
		if (parts[0].length < 2 || parts[1].length < 1) return false;
		return true;
	}
	if (trimmed.includes("/")) return false;
	return true;
}
