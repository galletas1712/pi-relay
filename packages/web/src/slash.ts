export interface SlashCommandInfo {
	name: string;
	description: string;
	argumentHint?: string;
}

export interface ParsedSlash {
	name: string;
	args: string;
}

export const COMMANDS: SlashCommandInfo[] = [
	{ name: "help", description: "Show slash commands." },
	{ name: "fork", description: "Fork this session at a historical boundary." },
	{ name: "switch", description: "Switch branches or edit a historical message." },
	{ name: "compact", description: "Request context compaction." },
	{ name: "system", description: "Show PI.md prompt template." },
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
		args: (match?.[2] ?? "").trim()
	};
}
