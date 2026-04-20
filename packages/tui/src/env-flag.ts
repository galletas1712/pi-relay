/**
 * Conventional boolean env-var parser.
 *
 * Treats `"1"`, `"true"`, `"yes"` (case-insensitive) as enabled. Any other
 * value — including `"0"`, `"false"`, `"no"`, empty string, or unset — is
 * treated as disabled.
 *
 * Deliberately duplicated from `@pi-relay/coding-agent`'s `src/utils/env-flag.ts`:
 * the tui package is a lower layer and cannot import from coding-agent without
 * inverting the dependency direction.
 */
export function isTruthyEnvFlag(value: string | undefined): boolean {
	if (!value) return false;
	const normalized = value.toLowerCase();
	return normalized === "1" || normalized === "true" || normalized === "yes";
}
