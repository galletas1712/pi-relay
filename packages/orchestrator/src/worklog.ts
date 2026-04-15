import { mkdirSync } from "node:fs";
import { appendFile, readFile } from "node:fs/promises";
import { dirname } from "node:path";
import { Type } from "@sinclair/typebox";
import type { Tool } from "@mariozechner/pi-ai";

export const WORKLOG_UPDATE_TOOL = {
	name: "worklog_update",
	description:
		"Append durable knowledge worth preserving for future work, such as insights, measurements, design decisions, or hard-to-reproduce commands.",
	parameters: Type.Object(
		{
			content: Type.String({
				description: "Markdown entry with durable knowledge only. Do not use it as a progress log.",
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
- conceptual insights you derived from the code, architecture, or behavior
- concrete findings like APIs, invariants, file paths that matter, line references, or code patterns
- measurements, counts, or test results
- benchmark results or other performance observations
- decisions you made and why
- non-obvious commands worth reusing later, especially if they are hard to reconstruct

Do not repeat the last entry.
Do not restate inherited context unless you verified or corrected it.
Do not use the worklog for step-by-step progress updates, routine status pings, or "I looked at X" notes.
Do not log ordinary file browsing, obvious commands, or temporary hypotheses that did not matter.
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
