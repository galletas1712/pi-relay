/**
 * Conventional boolean env-var parser.
 *
 * Treats `"1"`, `"true"`, `"yes"` (case-insensitive) as enabled. Any other
 * value — including `"0"`, `"false"`, `"no"`, empty string, or unset — is
 * treated as disabled.
 *
 * This is the canonical way to read boolean-flag env vars in pi-relay.
 * Value-typed env vars (e.g., `PI_CACHE_RETENTION="none"|"long"`) should use
 * strict string-equality and are not covered by this helper.
 */
export function isTruthyEnvFlag(value: string | undefined): boolean {
	if (!value) return false;
	const normalized = value.toLowerCase();
	return normalized === "1" || normalized === "true" || normalized === "yes";
}
