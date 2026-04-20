import { createHash } from "node:crypto";
import { mkdirSync, renameSync } from "node:fs";
import { appendFile, readFile, writeFile } from "node:fs/promises";
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
	 * Foundational/pinned entries bypass tombstoning and are emitted as a
	 * dedicated `<pinned-facts>` block at the top of every descendant's spawn
	 * prompt. Enforced cap: {@link MAX_PINNED_ENTRIES} live pins per file.
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

/**
 * Upper bound on how many live (non-tombstoned) pinned entries a single
 * worklog file may carry at once. Hitting the cap forces the next pin to
 * explicitly displace an existing pin via `replacesPinnedId`. Keeping the
 * number small reflects the intended use of pins (rare, foundational facts);
 * it also bounds the cost of the `<pinned-facts>` block injected into every
 * descendant's spawn prompt.
 */
export const MAX_PINNED_ENTRIES = 20;

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
						"Mark this entry as a pinned foundational fact. Pinned entries are injected first into every descendant's spawn prompt and bypass supersession filtering. Pins are rare (cap: 20 live pins per agent). Leave as false unless the content is a cross-cutting invariant.",
				}),
			),
			replacesPinnedId: Type.Optional(
				Type.String({
					description:
						"When setting pin:true after the cap (20) is reached, pass the entry_id of an existing pinned entry to displace. The replaced entry's pin flips to false on disk; it remains present as an audit-trail entry. Ignored when pin is false.",
				}),
			),
		},
		{ additionalProperties: false },
	),
} satisfies Tool;

/**
 * Tool exposed ONLY to the worklog fork (never to the main agent loop) for
 * flipping a previously pinned entry's `pin: false`. Use when a pinned fact
 * becomes outdated and should no longer appear in descendants' pinned-facts
 * block without leaving a tombstone.
 */
export const WORKLOG_UNPIN_TOOL = {
	name: "worklog_unpin",
	description:
		"Unpin a previously pinned worklog entry. Use when a pinned fact becomes outdated and you want to remove it from the pinned-facts block without leaving a tombstone. Only affects pins in your own worklog.",
	parameters: Type.Object(
		{
			entry_id: Type.String({
				description: "The entry_id of the pinned entry to unpin.",
			}),
		},
		{ additionalProperties: false },
	),
} satisfies Tool;

/**
 * Hard cap on the length (in characters) of a focus-pointer's \`content\`
 * field. Oversize payloads are truncated to this many characters with an
 * ellipsis marker — the tool deliberately accepts oversize input rather
 * than rejecting because models are imperfect character counters, but we
 * clamp aggressively so a runaway pointer never dominates a child's
 * spawn prompt.
 */
export const MAX_FOCUS_POINTER_CHARS = 500;

/**
 * Tool exposed ONLY to the worklog fork (never to the main agent loop).
 * Records a short "what am I working on right now" pointer that child
 * agents inherit in place of the 4KB \`<ancestor-recent-context>\`
 * transcript tail. Exactly one of \`worklog_update\`, \`worklog_unpin\`, or
 * \`set_focus_pointer\` is honored per fork turn — the first tool call
 * wins; any additional calls are logged as warnings and ignored.
 */
export const SET_FOCUS_POINTER_TOOL = {
	name: "set_focus_pointer",
	description:
		"Record a short current-focus pointer describing what you're working on RIGHT NOW. Child agents inherit this in place of the raw conversation tail, so keep it to 1–3 sentences that describe the immediate task, relevant files, and any in-flight decisions. Prefer this over worklog_update for ephemeral in-progress context. Only one of worklog_update, worklog_unpin, or set_focus_pointer per turn — choose the most valuable.",
	parameters: Type.Object(
		{
			content: Type.String({
				description: `1–3 sentences of current focus. Keep under ${MAX_FOCUS_POINTER_CHARS} characters; longer payloads are truncated with an ellipsis.`,
			}),
		},
		{ additionalProperties: false },
	),
} satisfies Tool;

/**
 * Clamp a focus-pointer content string to {@link MAX_FOCUS_POINTER_CHARS}.
 * Oversize input is truncated (not rejected) and a trailing \`...\` is
 * appended so callers can see the value was clipped. Returns the exact
 * string that should be persisted on \`AgentRecord.currentFocus\`.
 */
export function clampFocusPointerContent(content: string): { content: string; truncated: boolean } {
	if (content.length <= MAX_FOCUS_POINTER_CHARS) {
		return { content, truncated: false };
	}
	// Reserve 3 characters for the ellipsis so the final length never
	// exceeds MAX_FOCUS_POINTER_CHARS.
	const head = content.slice(0, Math.max(0, MAX_FOCUS_POINTER_CHARS - 3));
	return { content: `${head}...`, truncated: true };
}

/**
 * Tool exposed ONLY to the out-of-band compaction fork (never to the main
 * agent loop nor to the per-turn worklog fork). Collapses a batch of
 * older worklog entries into a single compact summary so the worklog
 * file doesn't grow unbounded. The compaction fork hands the model a
 * concrete list of older entries and asks it to produce a dense summary
 * that preserves file:line references, API names, measurements, and
 * concrete commands verbatim.
 */
export const WORKLOG_COMPACT_TOOL = {
	name: "compact_worklog",
	description:
		"Emit a single compact summary of the older worklog entries provided below. PRESERVE verbatim: file paths, line numbers, API names, measurements, concrete commands, symbol names. DO NOT paraphrase code references. Group related entries; drop stale status updates; keep durable insights.",
	parameters: Type.Object(
		{
			summary: Type.String({
				description:
					"Markdown summary. Use ## subsection headers for logical groupings. Preserve file:line refs, symbol names, API names, and concrete commands verbatim from the source entries. Aim for 1/5 to 1/10 the original size.",
			}),
		},
		{ additionalProperties: false },
	),
} satisfies Tool;

export function buildWorklogPrompt(
	lastEntry: string | undefined,
	topicVocabulary?: Array<{ slug: string; count: number }>,
	currentlyPinned?: Array<{ entry_id: string; summary: string }>,
	currentFocus?: { content: string; turn: number },
	currentSummary?: string,
): string {
	const topicSection = formatTopicVocabularySection(topicVocabulary);
	const pinnedSection = formatCurrentlyPinnedSection(currentlyPinned);
	const focusSection = formatCurrentFocusSection(currentFocus);
	const summarySection = formatCurrentSummarySection(currentSummary);
	return `Your worklog preserves knowledge for downstream consumption. Child agents inherit ancestor worklogs, and restored sessions reuse these entries after interruption.
${summarySection}<last-worklog-entry>
${lastEntry ?? "(no previous entries)"}
</last-worklog-entry>
${topicSection}${pinnedSection}${focusSection}
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
- supersedes: When your new entry corrects or replaces a prior entry (yours or an ancestor's), cite the prior entry_id (shown in the \`<last-worklog-entry>\` header comment, e.g. \`<!-- meta: {"entry_id":"abcd1234",...} -->\`) in supersedes. The system treats a superseded entry as a tombstone — child agents will no longer see it. Use this instead of writing "(supersedes prior entry)" in the body; the machine-readable field is what the system consumes.
- pin: Mark content as pinned (\`pin: true\`) only when it is a cross-cutting foundational invariant the entire agent tree will keep coming back to — architectural boundaries, persistent configuration, non-obvious system laws. Pins should be rare: at most ${MAX_PINNED_ENTRIES} across a long session. Do NOT pin per-task status, ephemeral measurements, or entries that will obviously become obsolete within a few turns. When a pinned fact becomes outdated, call \`worklog_unpin\` with its \`entry_id\`. When the pin cap is reached, pass \`replacesPinnedId\` in \`worklog_update\` with the entry_id of an existing pin to displace.

Focus pointer (set_focus_pointer):
- Call \`set_focus_pointer({ content })\` when you have no durable insight for \`worklog_update\`, but the agent's current focus has changed — a different task, switched files, a new goal. Child agents inherit this pointer in place of the raw conversation tail.
- Keep it to 1–3 sentences (≤${MAX_FOCUS_POINTER_CHARS} characters): the immediate task, the relevant files, any in-flight decision. Do NOT restate durable knowledge (use \`worklog_update\` for that).
- The prior pointer (if any) is shown above in \`<current-focus>\`. Skip re-emitting if nothing material has changed.
- Per turn, call AT MOST ONE of \`worklog_update\`, \`worklog_unpin\`, or \`set_focus_pointer\` — the first wins, the rest are logged as warnings and ignored. Choose the most valuable; many turns should call none.`;
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

function formatCurrentlyPinnedSection(
	currentlyPinned: Array<{ entry_id: string; summary: string }> | undefined,
): string {
	if (!currentlyPinned || currentlyPinned.length === 0) {
		return "";
	}
	const lines = currentlyPinned.map(
		({ entry_id, summary }) => `- ${entry_id} — ${summary}`,
	);
	return `\n<currently-pinned>\n${lines.join("\n")}\n</currently-pinned>\n`;
}

function formatCurrentFocusSection(
	currentFocus: { content: string; turn: number } | undefined,
): string {
	if (!currentFocus || !currentFocus.content) {
		return "";
	}
	// Rendered inside the fork prompt so the model sees the prior focus
	// pointer and can skip re-emitting when nothing material has changed.
	return `\n<current-focus turn="${currentFocus.turn}">\n${currentFocus.content}\n</current-focus>\n`;
}

function formatCurrentSummarySection(currentSummary: string | undefined): string {
	if (!currentSummary || currentSummary.length === 0) {
		return "";
	}
	// Rendered inside the fork prompt so the model sees the current
	// compaction summary and does not attempt to re-summarize content
	// that has already been distilled.
	return `\n<current-summary>\n${currentSummary}\n</current-summary>\n`;
}

/**
 * Build a short, stable summary line for a pinned entry — used by the fork
 * prompt's `<currently-pinned>` block so the model sees what's already
 * pinned and can pick one to displace via `replacesPinnedId`.
 *
 * First 80 characters of the body, collapsed to a single line.
 */
export function summarizePinnedEntry(entry: ParsedWorklogEntry): string {
	const oneLine = entry.body.replace(/\s+/g, " ").trim();
	if (oneLine.length <= 80) return oneLine;
	return `${oneLine.slice(0, 77)}...`;
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

/**
 * Rewrite a single worklog entry's `pin` meta field in place. Returns
 * `true` if the entry was found and rewritten; `false` if the id was not
 * present (no-op). Entries without an `entry_id` (legacy entries) are never
 * matched.
 *
 * Atomic: writes to `${filePath}.tmp` then `renameSync`-es over the target.
 * Mirrors the pattern used by `Orchestrator.persistTree` for tree.json.
 *
 * Preserves every other meta field (entry_id, topics, supersedes) exactly
 * and preserves the entry body. Legacy entries interleaved with structured
 * entries in a mixed-format file are preserved verbatim via their `raw`
 * text.
 *
 * Concurrency: callers MUST serialize writes to the same `filePath`
 * externally — the orchestrator enforces this via its per-agent
 * `pendingWorklogFork` promise chain so append and pin-rewrite paths never
 * race for the same worklog.
 */
export async function updateWorklogEntryPin(
	filePath: string,
	entryId: string,
	pin: boolean,
): Promise<boolean> {
	const content = await readWorklog(filePath);
	if (!content) return false;
	const entries = parseWorklogEntries(content);
	let target: ParsedWorklogEntry | undefined;
	for (const entry of entries) {
		if (entry.id === entryId) {
			target = entry;
			break;
		}
	}
	if (!target) return false;
	// Rebuild the target entry's raw text with the new pin value. Preserve
	// header iso/turn and body exactly; rewrite only the meta JSON.
	const newMeta: WorklogEntryMeta = { ...target.meta, pin };
	const newRaw = reserializeEntry(target, newMeta);
	const rebuilt = entries
		.map((entry) => (entry === target ? newRaw : entry.raw))
		.join("\n\n");
	const final = rebuilt.endsWith("\n") ? `${rebuilt}\n` : `${rebuilt}\n\n`;
	mkdirSync(dirname(filePath), { recursive: true });
	const tempFile = `${filePath}.tmp`;
	await writeFile(tempFile, final, "utf-8");
	renameSync(tempFile, filePath);
	return true;
}

function reserializeEntry(entry: ParsedWorklogEntry, meta: WorklogEntryMeta): string {
	const metaComment = `<!-- meta: ${serializeMeta(meta)} -->`;
	return `## Entry — ${entry.iso} (turn ${entry.turn}) ${metaComment}\n\n${entry.body}`;
}

/**
 * Header shape for compaction summaries:
 *   `## Summary — <iso> (compacted up to turn N) <!-- meta: {"entry_id":"...","compactedCount":K} -->`
 *
 * Lives at the TOP of a worklog file, above all `## Entry —` headers, so
 * it doesn't get captured by `ENTRY_BOUNDARY_REGEX` (which anchors on
 * `## Entry —`). `getLastWorklogEntry` and `parseWorklogEntries` both
 * ignore it; `getLastCompactionSummary` extracts it by looking for the
 * first-ever `## Summary —` line and reading everything up to the next
 * `## Entry —` or end of file.
 */
const SUMMARY_HEADER_REGEX =
	/^## Summary —\s+(?<iso>\S+)\s+\(compacted up to turn\s+(?<upTo>-?\d+)\)(?:\s+<!--\s*meta:\s*(?<meta>.*?)\s*-->)?\s*$/m;

export interface CompactionSummaryMeta {
	entry_id?: string;
	compactedCount?: number;
}

export interface ParsedCompactionSummary {
	/** ISO timestamp the summary was written. */
	iso: string;
	/** Turn number of the last original entry collapsed into this summary. */
	upToTurn: number;
	/** Parsed header meta, or `{}` if absent/invalid. */
	meta: CompactionSummaryMeta;
	/** Summary body (everything after the header, before the first `## Entry —`), trimmed. */
	body: string;
	/** Full summary block including header, preserving original bytes. */
	raw: string;
}

/**
 * Extract the most recent compaction summary from a worklog file's raw
 * content. Returns `undefined` for files without a summary (the common
 * case — every pre-PR-9 worklog file). When present the summary always
 * lives at the top of the file above all `## Entry —` headers.
 *
 * Defensive against malformed headers: returns `undefined` rather than
 * throwing when the `## Summary —` line can't be parsed.
 */
export function getLastCompactionSummary(content: string): ParsedCompactionSummary | undefined {
	if (!content) return undefined;
	const summaryIndex = content.indexOf("## Summary —");
	if (summaryIndex < 0) return undefined;
	// Find the end of the summary block: the first `## Entry —` header at a
	// line start, or end-of-file. Use indexOf + newline check because the
	// `## Entry —` token could legitimately appear inside a code block in
	// the summary body; we require it to start a line.
	let endIndex = content.length;
	const entryIndex = content.indexOf("\n## Entry —", summaryIndex);
	if (entryIndex >= 0) {
		endIndex = entryIndex;
	}
	const block = content.slice(summaryIndex, endIndex).trim();
	const firstNewline = block.indexOf("\n");
	const headerLine = firstNewline === -1 ? block : block.slice(0, firstNewline);
	const body = firstNewline === -1 ? "" : block.slice(firstNewline + 1).trim();
	const headerMatch = SUMMARY_HEADER_REGEX.exec(headerLine);
	if (!headerMatch || !headerMatch.groups) return undefined;
	const iso = headerMatch.groups.iso ?? "";
	const upToTurn = Number.parseInt(headerMatch.groups.upTo ?? "0", 10);
	const metaJson = headerMatch.groups.meta;
	let meta: CompactionSummaryMeta = {};
	if (metaJson !== undefined && metaJson.length > 0) {
		try {
			const parsed = JSON.parse(metaJson);
			if (parsed && typeof parsed === "object" && !Array.isArray(parsed)) {
				meta = parsed as CompactionSummaryMeta;
			}
		} catch {
			// Malformed meta: keep meta = {}, never throw.
		}
	}
	return {
		iso,
		upToTurn: Number.isFinite(upToTurn) ? upToTurn : 0,
		meta,
		body,
		raw: block,
	};
}

/**
 * Format a compaction-summary header line + body exactly how the
 * compaction path writes it to disk.
 */
export function formatCompactionSummary(
	summary: string,
	upToTurn: number,
	options?: { iso?: string; compactedCount?: number },
): string {
	const trimmed = summary.trim();
	const iso = options?.iso ?? new Date().toISOString();
	const meta: CompactionSummaryMeta = {
		entry_id: computeEntryId(trimmed, iso),
		compactedCount: options?.compactedCount ?? 0,
	};
	const metaComment = `<!-- meta: ${serializeMeta(meta as WorklogEntryMeta)} -->`;
	return `## Summary — ${iso} (compacted up to turn ${upToTurn}) ${metaComment}\n\n${trimmed}`;
}

/**
 * Build the prompt handed to the compaction LLM. Lists each older entry
 * with its header + body so the model sees entry boundaries, and
 * explicitly instructs verbatim preservation of file:line refs, API
 * names, measurements, and commands.
 */
export function buildCompactionPrompt(olderEntries: ParsedWorklogEntry[]): string {
	const body = olderEntries
		.map((entry) => {
			const idLabel = entry.id ?? "legacy";
			return `## Entry — ${entry.iso} (turn ${entry.turn}) entry_id=${idLabel}\n${entry.body}`;
		})
		.join("\n\n---\n\n");
	return `Compact the following ${olderEntries.length} older worklog entries into a single dense summary via the compact_worklog tool.

Rules:
- Preserve file:line references, API names, measurements, and concrete commands verbatim.
- DO NOT paraphrase code references.
- Group related findings under \`##\` subsection headers.
- Drop progress pings, superseded conclusions, and temporary hypotheses.
- Aim for 1/5 to 1/10 the original size. Concise but complete.
- If an entry states an invariant, preserve it word-for-word.

<older-entries>
${body}
</older-entries>`;
}

/**
 * Atomically rewrite a worklog file with (optionally) a new summary
 * block at the top, followed by a list of entries verbatim via their
 * `raw` bytes. The entries list should already be in the desired final
 * order; typical layout is: pinned entries (in original order) then the
 * most-recent-K preserved entries.
 *
 * Summary is omitted when `summary` is `undefined`. Preserves tombstone
 * trails: if the caller passes tombstoned entries through, they survive
 * on disk and continue to function as supersession pointers when the
 * file is re-parsed.
 *
 * Concurrency: same precondition as `updateWorklogEntryPin` — callers
 * MUST serialize writes to the same `filePath` externally. The
 * orchestrator enforces this via its per-agent `pendingWorklogFork`
 * promise chain so append, pin-rewrite, and compaction never race.
 */
export async function rewriteWorklogWithSummary(
	filePath: string,
	entries: ParsedWorklogEntry[],
	summary: string | undefined,
): Promise<void> {
	const parts: string[] = [];
	if (summary) {
		parts.push(summary);
	}
	for (const entry of entries) {
		parts.push(entry.raw);
	}
	const joined = parts.join("\n\n");
	const final = joined.endsWith("\n") ? `${joined}\n` : `${joined}\n\n`;
	mkdirSync(dirname(filePath), { recursive: true });
	const tempFile = `${filePath}.tmp`;
	await writeFile(tempFile, final, "utf-8");
	renameSync(tempFile, filePath);
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

/**
 * Collect live pinned entries from a parsed worklog. "Live" means the
 * entry's `pin === true` AND its id is not in the supplied tombstone set.
 * The cap enforced by {@link MAX_PINNED_ENTRIES} is counted against this
 * live set, not against every entry that was ever pinned in the file's
 * history.
 */
export function collectLivePinnedEntries(
	entries: ParsedWorklogEntry[],
	tombstones: ReadonlySet<string>,
): ParsedWorklogEntry[] {
	const pinned: ParsedWorklogEntry[] = [];
	for (const entry of entries) {
		if (entry.meta.pin !== true) continue;
		if (entry.id === undefined) continue; // legacy entry w/ pin:true is impossible but handled
		if (tombstones.has(entry.id)) continue;
		pinned.push(entry);
	}
	return pinned;
}

/**
 * Soft upper bound on the count of non-pinned entries emitted across all
 * ancestor files combined. Pinned entries are NEVER subject to the cap —
 * they always appear in the `<pinned-facts>` block regardless. The cap is
 * "soft" in that both a char budget and an entry-count budget are enforced
 * and whichever bites first wins.
 */
export const MAX_ANCESTOR_TAIL_ENTRIES = 15;

/**
 * Soft upper bound on the total character count of non-pinned entries
 * emitted across all ancestor files combined. Measured on the re-emitted
 * `raw` text of each surviving entry. Same semantics as
 * {@link MAX_ANCESTOR_TAIL_ENTRIES}: pinned entries bypass this cap.
 */
export const MAX_ANCESTOR_TAIL_CHARS = 20_000;

/**
 * Build the ancestor-worklog prefix injected into child agent spawns.
 *
 * Output shape (all sections optional; `\n\n`-separated):
 * 1. `<pinned-facts>` containing every live pinned entry across ALL
 *    ancestor files in ancestor order, then per-file entry order. Pinned
 *    entries BYPASS the tombstone, topic, and tail-cap filters — a pin is
 *    a stronger statement than any of those, so the block carries the
 *    entry regardless.
 * 2. Optional `<!-- truncated: dropped N older non-pinned entries -->`
 *    marker when the tail cap dropped entries.
 * 3. Per-file `<ancestor-worklog>` wrappers containing entries that are
 *    neither tombstoned, pinned, topic-filtered-out, nor dropped by the
 *    tail cap.
 *
 * Tombstone semantics: every parsed entry's `meta.supersedes` contributes
 * its ids to a single tombstone set that is unioned across ALL ancestor
 * files in this call. Any non-pinned entry whose `meta.entry_id` appears
 * in that set is dropped at read time. The file on disk is not modified —
 * the tombstone is applied only to child-visible context, preserving the
 * audit trail.
 *
 * Cross-file tombstoning is intentional: if the parent learned that a
 * grandparent fact was wrong, the child should not inherit the wrong fact.
 *
 * Topic filtering (`options.includeTopics`): when non-empty, a non-pinned
 * entry is emitted only if its `meta.topics` intersects the set OR it has
 * no topics at all (legacy / unlabeled entries bypass the filter — never
 * silently drop history that predates topic tagging). Pinned entries
 * bypass the filter unconditionally. An undefined or empty set disables
 * filtering (pre-PR-7 behavior).
 *
 * Tail cap ({@link MAX_ANCESTOR_TAIL_ENTRIES} /
 * {@link MAX_ANCESTOR_TAIL_CHARS}): applied across the combined
 * non-pinned surviving entries in ancestor order (root first, then
 * within-file order). The MOST RECENT entries are kept; older entries at
 * the head of the combined list are dropped. At least one entry is
 * always kept (the first-kept entry is allowed to exceed the char budget
 * on its own so tiny worklogs don't spuriously emit a truncation marker).
 * When truncation happens a comment marker is emitted ahead of the
 * per-file wrappers so the model knows context was trimmed.
 *
 * Edge cases:
 * - Legacy entries (no `entry_id`) can never be tombstoned and are never
 *   pinned (pin field absent => not pinned). They pass through the
 *   per-file section unchanged (subject to tail cap).
 * - Circular supersession (A→B and B→A) collapses both ids into the
 *   tombstone set, so both entries are dropped unless pinned.
 * - `supersedes` citing an unknown entry_id is a no-op.
 * - A pinned entry in the tombstone set still appears in `<pinned-facts>`.
 */
export async function buildAncestorWorklogPrefix(
	entries: Array<{ agentId: string; role: string; filePath: string }>,
	options?: { includeTopics?: ReadonlySet<string> },
): Promise<string> {
	// Pass 1: read + parse every file, collect the union of supersedes ids.
	type FileSection = {
		agentId: string;
		role: string;
		parsed: ParsedWorklogEntry[];
		summary: ParsedCompactionSummary | undefined;
	};
	const parsedPerFile: Array<FileSection | null> = [];
	const tombstones = new Set<string>();
	for (const entry of entries) {
		const content = await readWorklog(entry.filePath);
		if (!content.trim()) {
			parsedPerFile.push(null);
			continue;
		}
		const parsed = parseWorklogEntries(content);
		const summary = getLastCompactionSummary(content);
		for (const parsedEntry of parsed) {
			const supersedes = Array.isArray(parsedEntry.meta.supersedes)
				? parsedEntry.meta.supersedes
				: [];
			for (const id of supersedes) {
				if (typeof id === "string" && id.length > 0) {
					tombstones.add(id);
				}
			}
		}
		parsedPerFile.push({ agentId: entry.agentId, role: entry.role, parsed, summary });
	}

	// Pass 2a: collect pinned entries across all ancestors. Pinned entries
	// bypass the tombstone filter (pin is a stronger statement than a
	// supersession). Order: ancestor order (root first), then per-file entry
	// order.
	type PinnedSource = {
		agentId: string;
		role: string;
		entry: ParsedWorklogEntry;
	};
	const pinnedSources: PinnedSource[] = [];
	const pinnedIds = new Set<string>();
	for (const file of parsedPerFile) {
		if (!file) continue;
		for (const entry of file.parsed) {
			if (entry.meta.pin !== true) continue;
			if (entry.id === undefined) continue;
			pinnedSources.push({ agentId: file.agentId, role: file.role, entry });
			pinnedIds.add(entry.id);
		}
	}

	const sections: string[] = [];

	if (pinnedSources.length > 0) {
		const pinnedBlock = pinnedSources
			.map(({ agentId, role, entry }) => {
				const idAttr = entry.id ?? "";
				return `<entry agent="${agentId}" role="${role}" entry_id="${idAttr}">\n${entry.body}\n</entry>`;
			})
			.join("\n");
		sections.push(`<pinned-facts>\n${pinnedBlock}\n</pinned-facts>`);
	}

	// Pass 2b: apply supersession, pin-dedup, and topic filtering per file
	// to produce each file's candidate surviving list. Tombstoned entries
	// are dropped (pin beats tombstone — those entries already appeared in
	// `<pinned-facts>` above and are excluded from the per-file section).
	// Legacy entries (id === undefined) can never be tombstoned or pinned
	// and always survive the filter pass.
	//
	// Topic filter: an entry passes when `includeTopics` is absent/empty,
	// OR when its `meta.topics` is empty/missing (legacy/unlabeled — never
	// silently drop pre-tagging history), OR when topics intersect with
	// `includeTopics`. Pinned entries already bypassed this filter because
	// they were emitted in the `<pinned-facts>` block before this pass
	// runs.
	const includeTopics =
		options?.includeTopics && options.includeTopics.size > 0
			? options.includeTopics
			: undefined;

	type PerFileSurviving = {
		agentId: string;
		role: string;
		entries: ParsedWorklogEntry[];
		summary: ParsedCompactionSummary | undefined;
	};
	const perFileSurviving: PerFileSurviving[] = [];
	for (const file of parsedPerFile) {
		if (!file) continue;
		const surviving = file.parsed.filter((parsedEntry) => {
			// Pinned entries live only in `<pinned-facts>` — never in the
			// per-file wrapper.
			if (parsedEntry.id !== undefined && pinnedIds.has(parsedEntry.id)) {
				return false;
			}
			// Tombstone filter (legacy entries have no id and cannot be
			// tombstoned).
			if (parsedEntry.id !== undefined && tombstones.has(parsedEntry.id)) {
				return false;
			}
			// Topic filter. No filter / empty filter => include.
			if (!includeTopics) return true;
			const topics = Array.isArray(parsedEntry.meta.topics) ? parsedEntry.meta.topics : [];
			// Legacy / unlabeled: include (cannot silently drop
			// history that predates topic tagging).
			if (topics.length === 0) return true;
			for (const topic of topics) {
				if (typeof topic === "string" && includeTopics.has(topic)) {
					return true;
				}
			}
			return false;
		});
		perFileSurviving.push({ agentId: file.agentId, role: file.role, entries: surviving, summary: file.summary });
	}

	// Pass 2c: apply the global tail cap across the combined non-pinned
	// surviving entries. We keep the MOST RECENT entries (tail of the
	// ancestor-ordered list) up to both the entry-count budget and the
	// char budget; whichever bites first wins. The cap does NOT apply to
	// pinned entries — those already live in the separate `<pinned-facts>`
	// block.
	//
	// Combined order: root's entries first, then parent's, etc. in
	// ancestor order; within a file, existing entry order is preserved.
	// The tail is the end of this combined list, so older entries at the
	// head of the combined list are dropped first.
	type FlatEntry = { fileIndex: number; entry: ParsedWorklogEntry };
	const flat: FlatEntry[] = [];
	perFileSurviving.forEach((file, fileIndex) => {
		for (const entry of file.entries) {
			flat.push({ fileIndex, entry });
		}
	});

	const totalNonPinned = flat.length;
	let kept: FlatEntry[] = flat;
	if (flat.length > 0) {
		// Walk from the tail inward accumulating char count until a budget
		// binds. Entry-count budget caps at MAX_ANCESTOR_TAIL_ENTRIES; char
		// budget caps at MAX_ANCESTOR_TAIL_CHARS. To keep the guarantee
		// "always keep at least one entry" (so tiny worklogs with a single
		// huge entry don't get a spurious truncation marker), we allow the
		// first-kept entry to exceed the char budget on its own.
		const picked: FlatEntry[] = [];
		let charCount = 0;
		for (let i = flat.length - 1; i >= 0; i -= 1) {
			if (picked.length >= MAX_ANCESTOR_TAIL_ENTRIES) break;
			const raw = flat[i]?.entry.raw ?? "";
			if (picked.length > 0 && charCount + raw.length > MAX_ANCESTOR_TAIL_CHARS) {
				break;
			}
			picked.push(flat[i] as FlatEntry);
			charCount += raw.length;
		}
		picked.reverse();
		kept = picked;
	}
	const droppedCount = totalNonPinned - kept.length;
	if (droppedCount > 0) {
		sections.push(
			`<!-- truncated: dropped ${droppedCount} older non-pinned entries -->`,
		);
	}

	// Pass 3: regroup the kept entries back into per-file wrappers,
	// preserving ancestor order. A file with zero kept entries (because it
	// was fully tombstoned, fully topic-filtered, or dropped by the tail
	// cap) has its wrapper skipped.
	const keptByFile = new Map<number, ParsedWorklogEntry[]>();
	for (const { fileIndex, entry } of kept) {
		let bucket = keptByFile.get(fileIndex);
		if (!bucket) {
			bucket = [];
			keptByFile.set(fileIndex, bucket);
		}
		bucket.push(entry);
	}
	for (let fileIndex = 0; fileIndex < perFileSurviving.length; fileIndex += 1) {
		const file = perFileSurviving[fileIndex] as PerFileSurviving | undefined;
		if (!file) continue;
		const bucket = keptByFile.get(fileIndex);
		const hasEntries = bucket !== undefined && bucket.length > 0;
		const hasSummary = file.summary !== undefined;
		if (!hasEntries && !hasSummary) continue;
		// Inter-entry whitespace is normalized to exactly one blank line on
		// re-emit. Individual entries' `raw` bytes are preserved verbatim;
		// only the glue between adjacent entries is canonicalized. Benign
		// for legacy files that may have had irregular spacing.
		//
		// Compaction summaries (if any) are emitted at the top of the
		// per-file `<ancestor-worklog>` wrapper so each ancestor's file
		// remains self-contained: summary first, then surviving entries.
		// This preserves PR-10's cache-stable-prefix goals (each file's
		// content is a deterministic function of its on-disk state) and
		// keeps the spawn-prompt structure flat.
		const partsForFile: string[] = [];
		if (file.summary) {
			partsForFile.push(file.summary.raw);
		}
		if (hasEntries) {
			partsForFile.push((bucket as ParsedWorklogEntry[]).map((parsedEntry) => parsedEntry.raw).join("\n\n"));
		}
		const body = partsForFile.join("\n\n");
		sections.push(
			`<ancestor-worklog agent="${file.agentId}" role="${file.role}">\n${body}\n</ancestor-worklog>`,
		);
	}

	return sections.join("\n\n");
}
