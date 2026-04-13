import { mkdirSync } from "node:fs";
import { appendFile, readFile } from "node:fs/promises";
import { dirname } from "node:path";
import { Type } from "@sinclair/typebox";
import type { Tool } from "@mariozechner/pi-ai";

export const WORKLOG_UPDATE_TOOL = {
	name: "worklog_update",
	description:
		"Append a new entry to the worklog when you have meaningful new understanding, findings, measurements, or decisions to preserve.",
	parameters: Type.Object(
		{
			content: Type.String({
				description: "Markdown worklog entry. Use concise sections and focus on new knowledge only.",
			}),
		},
		{ additionalProperties: false },
	),
} satisfies Tool;

export function buildWorklogPrompt(lastEntry: string | undefined): string {
	return `Your worklog preserves knowledge for downstream consumption. Child agents inherit ancestor worklogs, and restored sessions reuse these entries after interruption.

<last-worklog-entry>
${lastEntry ?? "(no previous entries)"}
</last-worklog-entry>

If you have materially NEW knowledge since the last entry, call the worklog_update tool. Include:
- conceptual understanding you derived from the code or files you inspected
- concrete discoveries like file paths, APIs, line references, or code patterns
- measurements, counts, or test results
- corrections to earlier assumptions
- decisions you made and why

Do not repeat the last entry.
Do not restate inherited context unless you verified or corrected it.
Do not use the worklog for step-by-step progress updates or routine status pings.
Batch related findings into one entry instead of emitting one entry per small observation.
For short tasks, prefer a single substantial entry near the end.
Do not call the tool if nothing meaningful changed.`;
}

export function formatWorklogEntry(content: string, turn: number): string {
	const trimmed = content.trim();
	return `## Entry — ${new Date().toISOString()} (turn ${turn})\n\n${trimmed}`;
}

export async function appendWorklogEntry(filePath: string, content: string, turn: number): Promise<string> {
	mkdirSync(dirname(filePath), { recursive: true });
	const entry = formatWorklogEntry(content, turn);
	await appendFile(filePath, `${entry}\n\n`, "utf-8");
	return entry;
}

export async function readWorklog(filePath: string): Promise<string> {
	try {
		return await readFile(filePath, "utf-8");
	} catch {
		return "";
	}
}

export function getLastWorklogEntry(content: string): string | undefined {
	const matches = content.match(/## Entry —[\s\S]*?(?=\n## Entry —|\s*$)/g);
	if (!matches || matches.length === 0) {
		return undefined;
	}
	return matches[matches.length - 1]?.trim();
}

export async function buildAncestorWorklogPrefix(
	entries: Array<{ agentId: string; role: string; filePath: string }>,
): Promise<string> {
	const sections: string[] = [];
	for (const entry of entries) {
		const content = await readWorklog(entry.filePath);
		if (!content.trim()) {
			continue;
		}

		sections.push(`<ancestor-worklog agent="${entry.agentId}" role="${entry.role}">\n${content.trim()}\n</ancestor-worklog>`);
	}

	return sections.join("\n\n");
}
