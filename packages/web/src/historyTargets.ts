import { displayParentIdForEntry } from "./displayParent.ts";
import type { TranscriptEntry } from "./types.ts";

export function branchEntriesFor(
	entries: TranscriptEntry[],
	leafId: string | null,
): TranscriptEntry[] {
	if (!leafId) return [];
	const byId = new Map(entries.map((entry) => [entry.id, entry]));
	const branch: TranscriptEntry[] = [];
	const seen = new Set<string>();
	let cursor: string | null = leafId;
	while (cursor && !seen.has(cursor)) {
		const entry = byId.get(cursor);
		if (!entry) break;
		branch.push(entry);
		seen.add(cursor);
		cursor = displayParentIdForEntry(entry, byId);
	}
	return branch.reverse();
}
