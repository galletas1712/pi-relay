import { displayParentIdForEntry } from "../displayParent.ts";
import type { TranscriptEntry } from "../types.ts";

export function mergeEntryBodies(current: Map<string, TranscriptEntry>, entries: TranscriptEntry[]): Map<string, TranscriptEntry> {
	let next = current;
	for (const entry of entries) {
		const existing = current.get(entry.id);
		const merged = reusableEntry(existing, entry);
		if (existing === merged) continue;
		if (next === current) next = new Map(current);
		next.set(entry.id, merged);
	}
	return next;
}

function reusableEntry(existing: TranscriptEntry | undefined, incoming: TranscriptEntry): TranscriptEntry {
	if (!existing) return incoming;
	// Transcript rows are append-only and immutable. Reusing existing entry
	// objects when the durable identity matches keeps React transcript rows and
	// scroll bookkeeping stable across canonical `session.get` refreshes.
	if (
		existing.id === incoming.id &&
		existing.parent_id === incoming.parent_id &&
		existing.timestamp_ms === incoming.timestamp_ms &&
		existing.sequence === incoming.sequence
	) {
		return existing;
	}
	return incoming;
}

export function entryIds(entries: TranscriptEntry[]): string[] {
	return entries.map((entry) => entry.id);
}

export function activeBranchIdsForSnapshot(currentIds: string[], activeLeafId: string | null): string[] {
	if (!activeLeafId) return [];
	return currentIds.at(-1) === activeLeafId ? currentIds : [activeLeafId];
}

export function appendActiveBranchEntries(
	currentIds: string[],
	entriesById: Map<string, TranscriptEntry>,
	entries: TranscriptEntry[],
): string[] | null {
	if (entries.length === 0) return currentIds;
	let nextIds = currentIds;
	for (const entry of entries) {
		if (nextIds.includes(entry.id)) continue;
		const currentLeafId = nextIds.at(-1) ?? null;
		if (displayParentIdForEntry(entry, entriesById) !== currentLeafId) return null;
		if (nextIds === currentIds) nextIds = [...currentIds];
		nextIds.push(entry.id);
	}
	return nextIds;
}

export function selectedEntriesFromIds(ids: string[], entriesById: Map<string, TranscriptEntry>): TranscriptEntry[] {
	return ids.flatMap((id) => {
		const entry = entriesById.get(id);
		return entry ? [entry] : [];
	});
}

export function sameStringArray(left: string[], right: string[]): boolean {
	if (left.length !== right.length) return false;
	for (let index = 0; index < left.length; index += 1) {
		if (left[index] !== right[index]) return false;
	}
	return true;
}

export function uniqueStringArray(values: string[]): string[] {
	const seen = new Set<string>();
	const result: string[] = [];
	for (const value of values) {
		if (seen.has(value)) continue;
		seen.add(value);
		result.push(value);
	}
	return result;
}
