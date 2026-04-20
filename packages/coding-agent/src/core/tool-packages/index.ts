/**
 * Internal glue that turns `@pi-relay/tool-kit` `ToolProvider`s into
 * `ToolDefinition`s via the shared tool resolver.
 *
 * See `../../docs/tool-packages.md` for the design and milestone plan.
 */

export { createToolHost, createToolHostFactory } from "./host.js";
export {
	bashInterface,
	builtinToolInterfaces,
	defaultToolInterfaceRegistry,
	ToolInterfaceRegistry,
	webSearchInterface,
} from "./interfaces.js";
export { configureToolsInRegistry, registerToolProviderInExtension } from "./register.js";
export {
	defaultResolveProviderSecrets,
	defaultResolveToolConfig,
	type HostFactory,
	type RegisteredProvider,
	type ResolvedTool,
	type ResolveProviderSecrets,
	type ResolveToolConfig,
	ToolRegistry,
	type ToolRegistryOptions,
} from "./tools.js";
