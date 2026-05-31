import { memo } from "react";
import { LogHeader } from "./panels.tsx";
import type { ModelOption } from "./sessionDefaults.ts";
import { displayActivity, isArchivedSession, sessionTitle, type SessionDisplayInfo } from "./sessionList.ts";
import { MessageList } from "./transcript.tsx";
import type { TurnCardView } from "./transcript.tsx";
import type { ReasoningEffort, SessionSnapshot, TranscriptEntry } from "./types.ts";

export interface ChatPaneProps {
	session: SessionDisplayInfo | null;
	snapshot: SessionSnapshot | null;
	entries: TranscriptEntry[];
	turnCards?: TurnCardView[] | null;
	transcriptLoading: boolean;
	modelOptions: ModelOption[];
	modelValue: string;
	modelLocked: boolean;
	modelControlsDisabled: boolean;
	reasoningEfforts: ReasoningEffort[];
	reasoningEffort: ReasoningEffort;
	rightOpen: boolean;
	selectedId: string | null;
	resumingTurnId: string | null;
	onModelChange: (value: string) => void;
	onReasoningEffortChange: (value: ReasoningEffort) => void;
	onToggleRight: () => void;
	onResumeTurn: (entryId: string) => void;
	onExpandTurn?: (turnId: string) => void;
	loadingTurnId?: string | null;
}

export const ChatPane = memo(function ChatPane({
	session,
	snapshot,
	entries,
	turnCards,
	transcriptLoading,
	modelOptions,
	modelValue,
	modelLocked,
	modelControlsDisabled,
	reasoningEfforts,
	reasoningEffort,
	rightOpen,
	selectedId,
	resumingTurnId,
	onModelChange,
	onReasoningEffortChange,
	onToggleRight,
	onResumeTurn,
	onExpandTurn,
	loadingTurnId
}: ChatPaneProps) {
	const loadedLeafId = activeLeafIdFromEntries(entries);
	const visibleActiveLeafId = loadedLeafId ?? snapshot?.active_leaf_id ?? null;
	return (
		<main className="log-pane" data-slot="agent-log">
			<ChatHeader
				session={session}
				snapshot={snapshot}
				modelOptions={modelOptions}
				modelValue={modelValue}
				modelLocked={modelLocked}
				modelControlsDisabled={modelControlsDisabled}
				reasoningEfforts={reasoningEfforts}
				reasoningEffort={reasoningEffort}
				rightOpen={rightOpen}
				onModelChange={onModelChange}
				onReasoningEffortChange={onReasoningEffortChange}
				onToggleRight={onToggleRight}
			/>
			<MessageList
				entries={entries}
				turnCards={turnCards}
				pendingActions={snapshot?.pending_actions ?? []}
				activeLeafId={visibleActiveLeafId}
				isRunning={snapshot?.activity === "running"}
				serverTimeMs={snapshot?.server_time_ms ?? null}
				hasSession={!!selectedId}
				sessionId={selectedId}
				entriesSessionId={snapshot?.session_id ?? null}
				loadingSession={transcriptLoading}
				onResumeTurn={onResumeTurn}
				resumingTurnId={resumingTurnId}
				onExpandTurn={onExpandTurn}
				loadingTurnId={loadingTurnId}
			/>
		</main>
	);
});

export function activeLeafIdFromEntries(entries: TranscriptEntry[]): string | null {
	return entries.at(-1)?.id ?? null;
}

interface ChatHeaderProps {
	session: SessionDisplayInfo | null;
	snapshot: SessionSnapshot | null;
	modelOptions: ModelOption[];
	modelValue: string;
	modelLocked: boolean;
	modelControlsDisabled: boolean;
	reasoningEfforts: ReasoningEffort[];
	reasoningEffort: ReasoningEffort;
	rightOpen: boolean;
	onModelChange: (value: string) => void;
	onReasoningEffortChange: (value: ReasoningEffort) => void;
	onToggleRight: () => void;
}

const ChatHeader = memo(function ChatHeader({
	session,
	snapshot,
	modelOptions,
	modelValue,
	modelLocked,
	modelControlsDisabled,
	reasoningEfforts,
	reasoningEffort,
	rightOpen,
	onModelChange,
	onReasoningEffortChange,
	onToggleRight
}: ChatHeaderProps) {
	const archived = session ? isArchivedSession(session) : false;
	const modelDisabled = modelLocked || modelControlsDisabled;
	const displayedModelOptions = modelOptions.some((option) => option.id === modelValue)
		? modelOptions
		: [{ id: modelValue, label: modelValue }, ...modelOptions];
	const displayedEfforts = reasoningEfforts.includes(reasoningEffort)
		? reasoningEfforts
		: [reasoningEffort, ...reasoningEfforts];
	return (
		<LogHeader
			archived={archived}
			activity={session ? displayActivity(snapshot?.activity ?? session.activity) : null}
			title={session ? sessionTitle(session) : null}
			modelOptions={displayedModelOptions}
			modelValue={modelValue}
			modelDisabled={modelDisabled}
			modelDisabledTitle={modelLocked ? "model is locked after the first transcript entry" : "model"}
			reasoningEfforts={displayedEfforts}
			reasoningEffort={reasoningEffort}
			rightOpen={rightOpen}
			onModelChange={onModelChange}
			onReasoningEffortChange={onReasoningEffortChange}
			onToggleRight={onToggleRight}
		/>
	);
});
