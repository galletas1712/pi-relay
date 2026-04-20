/**
 * @pi-relay/tool-kit - author-facing surface for building pi-relay tools.
 *
 * Two-layer model:
 *   ToolInterface   - the contract (name, description, TypeBox params). What
 *                     the LLM sees when a single provider is auto-bound.
 *   ToolProvider    - a pluggable implementation of an interface (id,
 *                     implements, config, secrets, execute). Never surfaced
 *                     to the model \u2014 provider ids are internal.
 *   ToolsConfig     - user-authored map of LLM-visible tool names to chosen
 *                     providers (`pi.configureTools({...})`). One entry =
 *                     one tool name = one provider.
 *
 * See `README.md`. This main entry has no runtime dependency on any other
 * @pi-relay/* package; optional TUI render types live at the
 * `@pi-relay/tool-kit/render` subpath.
 */

export { defineToolInterface, defineToolProvider, isToolProvider } from "./define.js";
export { ToolConfigMissingError } from "./errors.js";
export type {
	LoginCallbacks,
	SecretKind,
	SecretSpec,
	ToolCallContext,
	ToolConfigEntry,
	ToolContent,
	ToolHost,
	ToolHostAuth,
	ToolHostModelRef,
	ToolImageContent,
	ToolInterface,
	ToolProvider,
	ToolProviderInitContext,
	ToolRenderer,
	ToolResult,
	ToolsConfig,
	ToolTextContent,
	ToolUpdateCallback,
} from "./types.js";
