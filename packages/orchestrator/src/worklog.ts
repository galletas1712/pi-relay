import { createHash } from "node:crypto";
import { mkdirSync } from "node:fs";
import { appendFile, readFile } from "node:fs/promises";
import { dirname } from "node:path";
import { Type } from "@sinclair/typebox";
import type { Tool } from "@pi-relay/ai";

/**
 * Structured metadata carried inline with each worklog entry header so
 * downstream tooling (supersession, pinning, topic filtering, compaction) can
 * operate on entries programmatically without re-deriving intent from prose.
 *
 * Stored as a compact JSON object inside an HTML comment on the same line as
 * the `## Entry — ...` header so the existing entry regex (which scans for
 * `## Entry —` line boundaries) keeps working and legacy entries without the
 * comment still parse.
 */
export interface WorklogEntryMeta {
	/**
	 * Stable 8-hex identifier for this entry, derived from the entry body and
	 * ISO timestamp. Other entries cite it via `supersedes`.
	 */
	entry_id?: string;
	/** Short slugs tagging the entry's subject area. */
	topics?: string[];
	/**
	 * entry_id values of prior entries this one corrects or replaces. Used by
	 * later PRs to tombstone superseded entries during ancestor injection.
	 */
	supersedes?: string[];
	/**
	 * Foundational/pinned entries bypass relevance filtering. Full pin
	 * semantics land in PR-6; this PR only plumbs the field so earlier
	 * structured entries round-trip correctly.
	 */
	pin?: boolean;
}

export interface ParsedWorklogEntry {
	/** `meta.entry_id` if present, else undefined (legacy entries). */
	id: string | undefined;
	/** ISO timestamp from the entry header. */
	iso: string;
	/** Turn number from the entry header. */
	turn: number;
	/** Parsed meta from the `<!-- meta: {...} -->` comment, or `{}` if absent/invalid. */
	meta: WorklogEntryMeta;
	/** Entry body (everything after the header line), trimmed. */
	body: string;
	/** Full entry text including header. Preserves the original formatting. */
	raw: string;
}

export const WORKLOG_UPDATE_TOOL = {
	name: "worklog_update",
	description:
		"Append durable knowledge worth preserving for future work, such as insights, measurements, design decisions, or hard-to-reproduce commands.",
	parameters: Type.Object(
		{
			content: Type.String({
				description: "Markdown entry with durable knowledge only. Do not use it as a progress log.",
			}),
			topics: Type.Optional(
				Type.Array(
					Type.String({ description: "Short slug tagging this entry's subject area." }),
					{
						description:
							"Zero or more short topic slugs (lowercase kebab-case, e.g. `caching/anthropic` or `orchestrator/restore`). Prefer reusing slugs already present in the worklog over coining new ones.",
					},
				),
			),
			supersedes: Type.Optional(
				Type.Array(
					Type.String({
						description: "entry_id of a prior worklog entry this one supersedes/contradicts.",
					}),
					{
						description:
							"entry_id values of earlier entries this entry corrects or replaces. Cite ids shown in the `<last-worklog-entry>` header comment.",
					},
				),
			),
			pin: Type.Optional(
				Type.Boolean({
					description:
						"Mark this entry as a pinned foundational fact. Use only for cross-cutting invariants; pins should be rare. Full pin semantics ship later — leave as false unless explicitly foundational.",
				}),
			),
		},
		{ additionalProperties: false },
	),
} satisfies Tool;

export function buildWorklogPrompt(
	lastEntry: string | undefined,
	topicVocabulary?: Array<{ slug: string; count: number }>,
): string {
	const topicSection = formatTopicVocabularySection(topicVocabulary);
	return `Your worklog preserves knowledge for downstream consumption. Child agents inherit ancestor worklogs, and restored sessions reuse these entries after interruption.

<last-worklog-entry>
${lastEntry ?? "(no previous entries)"}
</last-worklog-entry>
${topicSection}
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
Do not call the tool if nothing meaningful changed.

Additional structured fields on the tool:
- topics: Tag with one or more short slugs (lowercase kebab-case, e.g. \`caching/anthropic\` or \`orchestrator/restore\`). Prefer slugs already used in your prior entries (shown above when available).
- supersedes: If your entry corrects or replaces an earlier entry, cite its entry_id (shown in the \`<last-worklog-entry>\` header comment, e.g. \`<!-- meta: {"entry_id":"abcd1234",...} -->\`) in supersedes.
- pin: Leave as false unless the entry is explicitly foundational (a cross-cutting invariant). Full pin semantics ship later; pins should be rare.`;
}

function formatTopicVocabularySection(
	topicVocabulary: Array<{ slug: string; count: number }> | undefined,
): string {
	if (!topicVocabulary || topicVocabulary.length === 0) {
		return "";
	}
	const lines = topicVocabulary.map(({ slug, count }) => `- ${slug} (${count})`);
	return `\n<topic-vocabulary>\n${lines.join("\n")}\n</topic-vocabulary>\n`;
}

function computeEntryId(content: string, iso: string): string {
	return createHash("sha1").update(`${content}\n${iso}`).digest("hex").slice(0, 8);
}

function serializeMeta(meta: WorklogEntryMeta): string {
	// Compact JSON so the header line stays a single line.
	const serialized = JSON.stringify(meta);
	if (serialized.includes("-->")) {
		throw new Error(
			"Worklog entry meta contains an HTML-comment terminator ('-->'); refusing to serialize to avoid corrupting the worklog file.",
		);
	}
	return serialized;
}

/**
 * Format a worklog entry with structured meta. The `<!-- meta: ... -->` block
 * lives on the same line as the `## Entry —` header so the existing entry
 * boundary regex (matches on `^## Entry — `) still splits entries correctly.
 *
 * `iso` defaults to the current time; tests and deterministic-ID callers may
 * pass an explicit timestamp.
 */
export function formatWorklogEntry(
	content: string,
	turn: number,
	options?: {
		iso?: string;
		topics?: string[];
		supersedes?: string[];
		pin?: boolean;
	},
): string {
	const trimmed = content.trim();
	const iso = options?.iso ?? new Date().toISOString();
	const meta: WorklogEntryMeta = {
		entry_id: computeEntryId(trimmed, iso),
		topics: options?.topics ?? [],
		supersedes: options?.supersedes ?? [],
		pin: options?.pin ?? false,
	};
	const metaComment = `<!-- meta: ${serializeMeta(meta)} -->`;
	return `## Entry — ${iso} (turn ${turn}) ${metaComment}\n\n${trimmed}`;
}

export async function appendWorklogEntry(
	filePath: string,
	content: string,
	turn: number,
	meta?: { topics?: string[]; supersedes?: string[]; pin?: boolean },
): Promise<string> {
	mkdirSync(dirname(filePath), { recursive: true });
	const entry = formatWorklogEntry(content, turn, meta);
	// Ensure the file ends with a blank line before appending so the new
	// `## Entry —` header lands on its own line. Legacy files written before
	// this PR and files interrupted mid-write may be missing the trailing
	// separator; without this guard the two entries would concatenate into one
	// malformed header and break `parseWorklogEntries`.
	const existing = await readWorklog(filePath);
	const separator = existing.length === 0 || existing.endsWith("\n\n") ? "" : existing.endsWith("\n") ? "\n" : "\n\n";
	await appendFile(filePath, `${separator}${entry}\n\n`, "utf-8");
	return entry;
}

export async function readWorklog(filePath: string): Promise<string> {
	try {
		return await readFile(filePath, "utf-8");
	} catch {
		return "";
	}
}

/**
 * Regex that matches a full entry including its optional meta comment and
 * body. Uses a multiline start anchor so we split on `## Entry —` *line
 * starts* only (mid-body `## Entry —` references inside code blocks won't
 * match). Captures run to the next entry header or end-of-file.
 */
const ENTRY_BOUNDARY_REGEX = /## Entry —[\s\S]*?(?=\n## Entry —|\s*$)/g;

const HEADER_REGEX =
	/^## Entry —\s+(?<iso>\S+)\s+\(turn\s+(?<turn>\d+)\)(?:\s+<!--\s*meta:\s*(?<meta>.*?)\s*-->)?\s*$/;

/**
 * Parse a worklog file into structured entries. Legacy entries (no `<!--
 * meta: ... -->` header comment) parse cleanly with `meta: {}, id: undefined`.
 * Malformed meta JSON is tolerated — the entry still parses, meta stays `{}`.
 */
export function parseWorklogEntries(content: string): ParsedWorklogEntry[] {
	if (!content) return [];
	const rawMatches = content.match(ENTRY_BOUNDARY_REGEX) ?? [];
	const entries: ParsedWorklogEntry[] = [];
	for (const raw of rawMatches) {
		const trimmed = raw.trim();
		const firstNewline = trimmed.indexOf("\n");
		const headerLine = firstNewline === -1 ? trimmed : trimmed.slice(0, firstNewline);
		const bodyText = firstNewline === -1 ? "" : trimmed.slice(firstNewline + 1).trim();
		const headerMatch = HEADER_REGEX.exec(headerLine);
		if (!headerMatch || !headerMatch.groups) {
			// Unexpected header shape — skip rather than throw, to stay
			// forward-compatible with future header variants.
			continue;
		}
		const iso = headerMatch.groups.iso ?? "";
		const turn = Number.parseInt(headerMatch.groups.turn ?? "0", 10);
		const metaJson = headerMatch.groups.meta;
		let meta: WorklogEntryMeta = {};
		if (metaJson !== undefined && metaJson.length > 0) {
			try {
				const parsed = JSON.parse(metaJson);
				if (parsed && typeof parsed === "object" && !Array.isArray(parsed)) {
					meta = parsed as WorklogEntryMeta;
				}
			} catch {
				// Malformed meta: keep meta = {}, id = undefined. Never throw.
			}
		}
		entries.push({
			id: typeof meta.entry_id === "string" ? meta.entry_id : undefined,
			iso,
			turn: Number.isFinite(turn) ? turn : 0,
			meta,
			body: bodyText,
			raw: trimmed,
		});
	}
	return entries;
}

export function getLastWorklogEntry(content: string): string | undefined {
	const matches = content.match(ENTRY_BOUNDARY_REGEX);
	if (!matches || matches.length === 0) {
		return undefined;
	}
	return matches[matches.length - 1]?.trim();
}

/**
 * Compute a topic vocabulary (slug -> count) from a worklog's parsed entries,
 * ranked by count descending. Used by the fork prompt to hint at existing
 * slugs so the model reuses them instead of coining near-duplicates.
 */
export function computeTopicVocabulary(
	entries: ParsedWorklogEntry[],
	options?: { limit?: number },
): Array<{ slug: string; count: number }> {
	const limit = options?.limit ?? 30;
	const counts = new Map<string, number>();
	for (const entry of entries) {
		const topics = Array.isArray(entry.meta.topics) ? entry.meta.topics : [];
		for (const raw of topics) {
			if (typeof raw !== "string") continue;
			const slug = raw.trim();
			if (!slug) continue;
			counts.set(slug, (counts.get(slug) ?? 0) + 1);
		}
	}
	const ranked = Array.from(counts.entries()).map(([slug, count]) => ({ slug, count }));
	// Stable ordering: count desc, slug asc for tie-breaks.
	ranked.sort((a, b) => (b.count - a.count) || a.slug.localeCompare(b.slug));
	return ranked.slice(0, limit);
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
