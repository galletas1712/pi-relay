import type { SessionSnapshot, TranscriptEntry, TranscriptTreeNode, TurnCard } from "../types.ts";

export interface SelectedSessionCache {
	sessionId: string | null;
	snapshot: SessionSnapshot | null;
	activeBranchEntryIds: string[];
	entriesById: Map<string, TranscriptEntry>;
	treeNodesById: Map<string, TranscriptTreeNode>;
	treeChildrenByParentId: Map<string | null, string[]>;
	treeOrder: string[];
	treeActiveLeafId: string | null;
	treeTranscriptRevision: number | null;
	treeLoadedPrefixSequence: number;
	treeMaxSequence: number;
	treeComplete: boolean;
	turnCardsById: Map<string, TurnCard>;
	turnOrder: string[];
	turnDetailsById: Map<string, string[]>;
	transcriptTurnsLoaded: boolean;
	turnPageHydrationRevision: number;
	turnTranscriptRevision: number | null;
	turnActiveLeafId: string | null;
	turnHasMoreBefore: boolean;
	turnBeforeEntryId: string | null;
}
