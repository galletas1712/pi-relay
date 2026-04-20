/**
 * Errors thrown by tool providers and the coding-agent adapter.
 */

/**
 * Thrown by the adapter (or a provider's own `execute`) when required
 * configuration or a required secret is missing at call time. The message
 * is user-facing and the eventual `/configure <toolName>` UX surfaces it
 * directly.
 *
 * `providerId` is the provider id (e.g. "com.perplexity.sonar").
 */
export class ToolConfigMissingError extends Error {
	readonly providerId: string;
	readonly missing: readonly string[];

	constructor(providerId: string, missing: readonly string[], details?: string) {
		const keys = missing.join(", ");
		const prefix = `Tool provider "${providerId}" is missing required configuration: ${keys}`;
		super(details ? `${prefix}. ${details}` : prefix);
		this.name = "ToolConfigMissingError";
		this.providerId = providerId;
		this.missing = [...missing];
	}
}
