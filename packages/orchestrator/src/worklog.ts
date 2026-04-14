import { mkdirSync } from "node:fs";
import { appendFile, readFile } from "node:fs/promises";
import { dirname } from "node:path";
import { Type } from "@sinclair/typebox";
import type { Tool } from "@mariozechner/pi-ai";
import { DEFAULT_COMPACTION_SETTINGS, estimateTokens, shouldCompact } from "@mariozechner/pi-coding-agent";

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

export const WORKLOG_COMPACTION_TOOL = {
	name: "worklog_compaction",
	description: "Compact older worklog content into a shorter durable summary for downstream child-agent spawns.",
	parameters: Type.Object(
		{
			summary: Type.String({
				description:
					"Markdown summary that preserves durable facts from the compacted worklog history while staying materially shorter than the source.",
			}),
		},
		{ additionalProperties: false },
	),
} satisfies Tool;

export const WORKLOG_COMPACTION_SYSTEM_PROMPT =
	"You compact older worklog history for future spawned child agents. Output only by calling the provided tool.";

const WORKLOG_SECTION_REGEX = /## (?:Entry|Summary) —[\s\S]*?(?=\n## (?:Entry|Summary) —|\s*$)/g;

export function buildWorklogPrompt(lastEntry: string | undefined): string {
	return `Your worklog preserves knowledge for downstream consumption. Child agents inherit compacted ancestor worklogs derived from these entries, and restored sessions reuse them after interruption.

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

export function formatWorklogSummary(summary: string, turn: number): string {
	return `## Summary — ${new Date().toISOString()} (turn ${turn})\n\n${summary.trim()}`;
}

export async function appendWorklogEntry(filePath: string, content: string, turn: number): Promise<string> {
	mkdirSync(dirname(filePath), { recursive: true });
	const entry = formatWorklogEntry(content, turn);
	await appendFile(filePath, `${entry}\n\n`, "utf-8");
	return entry;
}

export async function appendWorklogSection(filePath: string, section: string): Promise<void> {
	mkdirSync(dirname(filePath), { recursive: true });
	await appendFile(filePath, `${section.trim()}\n\n`, "utf-8");
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

function getWorklogSections(content: string): string[] {
	return (content.match(WORKLOG_SECTION_REGEX) ?? []).map((section) => section.trim());
}

function isSummarySection(section: string): boolean {
	return section.startsWith("## Summary —");
}

function getSectionBody(section: string): string {
	const dividerIndex = section.indexOf("\n\n");
	if (dividerIndex === -1) {
		return "";
	}
	return section.slice(dividerIndex + 2).trim();
}

export function getWorklogEntries(content: string): string[] {
	return getWorklogSections(content).filter((section) => !isSummarySection(section));
}

export function getWorklogTurn(section: string): number | undefined {
	const match = section.match(/\(turn (\d+)\)/);
	if (!match) {
		return undefined;
	}
	return Number.parseInt(match[1]!, 10);
}

export function parseCompactedWorklog(content: string): { summary?: string; entries: string[] } {
	let summary: string | undefined;
	const entries: string[] = [];
	for (const section of getWorklogSections(content)) {
		if (isSummarySection(section)) {
			summary = getSectionBody(section);
			continue;
		}
		entries.push(section);
	}
	return { summary, entries };
}

export function renderCompactedWorklog(summarySection: string | undefined, entries: string[]): string {
	const sections = [
		summarySection?.trim(),
		...entries.map((entry) => entry.trim()),
	].filter((section): section is string => Boolean(section));
	return sections.join("\n\n");
}

export function estimateWorklogTokens(content: string): number {
	return estimateTokens({
		role: "user",
		content: [{ type: "text", text: content }],
		timestamp: 0,
	});
}

export function shouldCompactText(content: string, contextWindow: number): boolean {
	if (!content.trim()) {
		return false;
	}
	return shouldCompact(estimateWorklogTokens(content), contextWindow, DEFAULT_COMPACTION_SETTINGS);
}

export function shouldCompactWorklog(content: string, contextWindow: number): boolean {
	return shouldCompactText(content, contextWindow);
}

export function selectWorklogEntriesToKeep(
	entries: string[],
	keepRecentTokens: number,
): { compactedEntries: string[]; keptEntries: string[] } {
	if (entries.length === 0) {
		return { compactedEntries: [], keptEntries: [] };
	}

	let keptTokens = 0;
	let keepStartIndex = entries.length - 1;
	for (let index = entries.length - 1; index >= 0; index -= 1) {
		const sectionTokens = estimateWorklogTokens(entries[index]!);
		if (index < entries.length - 1 && keptTokens + sectionTokens > keepRecentTokens) {
			break;
		}
		keptTokens += sectionTokens;
		keepStartIndex = index;
	}

	return {
		compactedEntries: entries.slice(0, keepStartIndex),
		keptEntries: entries.slice(keepStartIndex),
	};
}

export function buildWorklogCompactionPrompt(
	previousSummary: string | undefined,
	compactedEntries: string[],
	recentContext?: string,
): string {
	const sections = [
		"You are compacting older worklog content that downstream child agents will inherit.",
		"",
		"Call `worklog_compaction` with one merged markdown summary that:",
		"- preserves durable facts only: decisions, invariants, APIs, exact file paths, tests, measurements, commands, gotchas, and constraints",
		"- merges the previous summary, the older raw entries, and any recent ancestor context without duplicating points",
		"- drops chronology, routine progress notes, and temporary hypotheses",
		"- stays materially shorter than the source so it fits comfortably in future spawn prompts",
		"- does not mention that compaction happened or refer to 'previous summary' / 'older entries'",
		"",
		"<previous-summary>",
		previousSummary ?? "(none)",
		"</previous-summary>",
		"",
		"<older-worklog-entries>",
		compactedEntries.join("\n\n") || "(none)",
		"</older-worklog-entries>",
		"",
		"<recent-ancestor-context>",
		recentContext ?? "(none)",
		"</recent-ancestor-context>",
	];
	return sections.join("\n");
}

export async function buildAncestorWorklogPrefix(
	entries: Array<{ agentId: string; role: string; filePath: string; fallbackFilePath?: string }>,
): Promise<string> {
	const sections: string[] = [];
	for (const entry of entries) {
		let content = await readWorklog(entry.filePath);
		if (!content.trim() && entry.fallbackFilePath) {
			content = await readWorklog(entry.fallbackFilePath);
		}
		if (!content.trim()) {
			continue;
		}

		sections.push(`<ancestor-worklog agent="${entry.agentId}" role="${entry.role}">\n${content.trim()}\n</ancestor-worklog>`);
	}

	return sections.join("\n\n");
}
