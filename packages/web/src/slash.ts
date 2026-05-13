export interface SlashCommandInfo {
	name: string;
	description: string;
	argumentHint?: string;
	requiresArgs?: boolean;
}

export interface ParsedSlash {
	name: string;
	args: string;
	raw: string;
}

export const COMMANDS: SlashCommandInfo[] = [
	{ name: "help", description: "Show slash commands." },
	{ name: "new", description: "Create and select a session.", argumentHint: "[title]" },
	{ name: "fork", description: "Open the fork picker.", argumentHint: "[title]" },
	{ name: "switch", description: "Switch branches or edit a historical message." },
	{ name: "compact", description: "Request context compaction." },
	{ name: "system", description: "Read or set global system prompt.", argumentHint: "[clear|prompt...]" },
	{ name: "rename", description: "Rename the selected session.", argumentHint: "<title>" },
	{ name: "archive", description: "Archive the selected idle session." },
	{ name: "unarchive", description: "Unarchive the selected session." },
	{ name: "provider", description: "Read or set session provider.", argumentHint: "[kind model]" },
	{ name: "export", description: "Export assistant messages from the current branch." }
];

export function filterCommands(query: string): SlashCommandInfo[] {
	const q = query.toLowerCase();
	if (!q) return COMMANDS.slice();
	return COMMANDS.filter((command) => command.name.startsWith(q));
}

export function findCommand(name: string): SlashCommandInfo | undefined {
	const normalized = name.toLowerCase();
	return COMMANDS.find((command) => command.name === normalized);
}

export function matchSlashPrefix(input: string): string | null {
	const match = input.match(/^\/(\S*)$/);
	return match ? (match[1] ?? "").toLowerCase() : null;
}

export function parseSlash(input: string): ParsedSlash | null {
	const trimmed = input.trim();
	if (!trimmed.startsWith("/")) return null;
	const match = trimmed.match(/^\/([^\s]*)(?:\s+([\s\S]*))?$/);
	return {
		name: (match?.[1] ?? "").toLowerCase(),
		args: (match?.[2] ?? "").trim(),
		raw: trimmed
	};
}
