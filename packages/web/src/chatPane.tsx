import { memo } from "react";
import { LogHeader } from "./panels.tsx";
import type { ModelOption } from "./sessionDefaults.ts";
import { isArchivedSession, sessionStatusWithDelegations, sessionTitle, type SessionDisplayInfo } from "./sessionList.ts";
import { MessageList } from "./transcript.tsx";
import type { TurnCardView } from "./transcript.tsx";
import type { ReasoningEffort, SessionSnapshot, TranscriptEntry } from "./types.ts";

export interface ChatPaneProps {
	session: SessionDisplayInfo | null;
	snapshot: SessionSnapshot | null;
	entries: TranscriptEntry[];
	turnCards?: TurnCardView[] | null;
	transcriptLoading: boolean;
	hasRunningDelegations: boolean;
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
	onNewSession: () => void;
	onResumeTurn: (entryId: string) => void;
	onExpandTurn?: (turnId: string) => void;
	onCollapseTurn?: (turnId: string) => void;
	loadingTurnId?: string | null;
	hasOlderTurns?: boolean;
	loadingOlderTurns?: boolean;
	onLoadOlderTurns?: () => void;
}

export const ChatPane = memo(function ChatPane({
	session,
	snapshot,
	entries,
	turnCards,
	transcriptLoading,
	hasRunningDelegations,
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
	onNewSession,
	onResumeTurn,
	onExpandTurn,
	onCollapseTurn,
	loadingTurnId,
	hasOlderTurns,
	loadingOlderTurns,
	onLoadOlderTurns
}: ChatPaneProps) {
	const loadedLeafId = activeLeafIdFromEntries(entries);
	const visibleActiveLeafId = loadedLeafId ?? snapshot?.active_leaf_id ?? null;
	return (
		<main className="log-pane" data-slot="agent-log">
			<ChatHeader
				session={session}
				snapshot={snapshot}
				hasRunningDelegations={hasRunningDelegations}
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
				onNewSession={onNewSession}
				onResumeTurn={onResumeTurn}
				resumingTurnId={resumingTurnId}
				onExpandTurn={onExpandTurn}
				onCollapseTurn={onCollapseTurn}
				loadingTurnId={loadingTurnId}
				hasOlderTurns={hasOlderTurns}
				loadingOlderTurns={loadingOlderTurns}
				onLoadOlderTurns={onLoadOlderTurns}
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
	hasRunningDelegations: boolean;
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
	hasRunningDelegations,
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
			status={session ? sessionStatusWithDelegations(snapshot?.activity ?? session.activity, hasRunningDelegations) : null}
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
