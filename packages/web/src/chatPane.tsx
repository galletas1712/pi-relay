import { memo, type ReactNode } from "react";
import { LogHeader } from "./panels.tsx";
import type { ModelOption } from "./sessionDefaults.ts";
import { isArchivedSession, sessionStatusWithDelegations, sessionTitle, type SessionDisplayInfo } from "./sessionList.ts";
import { MessageList } from "./transcript.tsx";
import type {
	OlderTurnsLoadRequest,
	OlderTurnsLoadResult,
	TranscriptDestination,
	TranscriptTurnPageIdentity,
	TurnCardView,
} from "./transcript.tsx";
import type { ReasoningEffort, SessionSnapshot, TranscriptEntry } from "./types.ts";

export interface ChatPaneProps {
	session: SessionDisplayInfo | null;
	snapshot: SessionSnapshot | null;
	entries: TranscriptEntry[];
	turnCards?: TurnCardView[] | null;
	transcriptLoading: boolean;
	transcriptError: string | null;
	transcriptErrorHasUsableCache: boolean;
	transcriptRetrying: boolean;
	hasRunningDelegations: boolean;
	modelOptions: ModelOption[];
	modelValue: string;
	modelLocked: boolean;
	modelControlsDisabled: boolean;
	reasoningControlsDisabled: boolean;
	mutationBlockedReason?: string | null;
	remoteReadBlockedReason?: string | null;
	reasoningEfforts: ReasoningEffort[];
	reasoningEffort: ReasoningEffort;
	rightOpen: boolean;
	selectedId: string | null;
	resumingTurnId: string | null;
	onModelChange: (value: string) => void;
	onReasoningEffortChange: (value: ReasoningEffort) => void;
	onSelectSession?: (sessionId: string) => void;
	onToggleRight: () => void;
	onNewSession: () => void;
	onResumeTurn: (entryId: string) => void;
	onExpandTurn?: (turnId: string) => void;
	onCollapseTurn?: (turnId: string) => void;
	loadingTurnId?: string | null;
	hasOlderTurns?: boolean;
	loadingOlderTurns?: boolean;
	onLoadOlderTurns?: (request: OlderTurnsLoadRequest) => Promise<OlderTurnsLoadResult>;
	transcriptDestination?: TranscriptDestination | null;
	transcriptTurnPageIdentity?: TranscriptTurnPageIdentity | null;
	onAcknowledgeTranscriptDestination?: (destinationId: number) => void;
	onRetryTranscript: () => void;
	routeNotice?: ReactNode;
}

export const ChatPane = memo(function ChatPane({
	session,
	snapshot,
	entries,
	turnCards,
	transcriptLoading,
	transcriptError,
	transcriptErrorHasUsableCache,
	transcriptRetrying,
	hasRunningDelegations,
	modelOptions,
	modelValue,
	modelLocked,
	modelControlsDisabled,
	reasoningControlsDisabled,
	mutationBlockedReason,
	remoteReadBlockedReason,
	reasoningEfforts,
	reasoningEffort,
	rightOpen,
	selectedId,
	resumingTurnId,
	onModelChange,
	onReasoningEffortChange,
	onSelectSession,
	onToggleRight,
	onNewSession,
	onResumeTurn,
	onExpandTurn,
	onCollapseTurn,
	loadingTurnId,
	hasOlderTurns,
	loadingOlderTurns,
	onLoadOlderTurns,
	transcriptDestination,
	transcriptTurnPageIdentity,
	onAcknowledgeTranscriptDestination,
	onRetryTranscript,
	routeNotice,
}: ChatPaneProps) {
	const loadedLeafId = activeLeafIdFromEntries(entries);
	const visibleActiveLeafId = loadedLeafId ?? snapshot?.active_leaf_id ?? null;
	return (
		<main className="log-pane" data-slot="agent-log">
			{routeNotice}
			<ChatHeader
				session={session}
				snapshot={snapshot}
				hasRunningDelegations={hasRunningDelegations}
				modelOptions={modelOptions}
				modelValue={modelValue}
				modelLocked={modelLocked}
				modelControlsDisabled={modelControlsDisabled}
				reasoningControlsDisabled={reasoningControlsDisabled}
				mutationBlockedReason={mutationBlockedReason}
				reasoningEfforts={reasoningEfforts}
				reasoningEffort={reasoningEffort}
				rightOpen={rightOpen}
				onModelChange={onModelChange}
				onReasoningEffortChange={onReasoningEffortChange}
				onSelectSession={onSelectSession}
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
				sessionError={transcriptError}
				sessionErrorHasUsableCache={transcriptErrorHasUsableCache}
				retryingSession={transcriptRetrying}
				onRetrySession={onRetryTranscript}
				onNewSession={onNewSession}
				onResumeTurn={onResumeTurn}
				resumingTurnId={resumingTurnId}
				resumeBlockedReason={mutationBlockedReason}
				remoteReadBlockedReason={remoteReadBlockedReason}
				onExpandTurn={onExpandTurn}
				onCollapseTurn={onCollapseTurn}
				loadingTurnId={loadingTurnId}
				hasOlderTurns={hasOlderTurns}
				loadingOlderTurns={loadingOlderTurns}
				onLoadOlderTurns={onLoadOlderTurns}
				destination={transcriptDestination}
				turnPageIdentity={transcriptTurnPageIdentity}
				onAcknowledgeDestination={onAcknowledgeTranscriptDestination}
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
	reasoningControlsDisabled: boolean;
	mutationBlockedReason?: string | null;
	reasoningEfforts: ReasoningEffort[];
	reasoningEffort: ReasoningEffort;
	rightOpen: boolean;
	onModelChange: (value: string) => void;
	onReasoningEffortChange: (value: ReasoningEffort) => void;
	onSelectSession?: (sessionId: string) => void;
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
	reasoningControlsDisabled,
	mutationBlockedReason,
	reasoningEfforts,
	reasoningEffort,
	rightOpen,
	onModelChange,
	onReasoningEffortChange,
	onSelectSession,
	onToggleRight
}: ChatHeaderProps) {
	const archived = session ? isArchivedSession(session) : false;
	const modelDisabled = modelLocked || modelControlsDisabled || !!mutationBlockedReason;
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
			parentSessionId={snapshot?.parent_session_id ?? null}
			modelOptions={displayedModelOptions}
			modelValue={modelValue}
			modelDisabled={modelDisabled}
			modelLocked={modelLocked}
			reasoningDisabled={reasoningControlsDisabled || !!mutationBlockedReason}
			controlsBlockedReason={mutationBlockedReason}
			reasoningEfforts={displayedEfforts}
			reasoningEffort={reasoningEffort}
			rightOpen={rightOpen}
			onModelChange={onModelChange}
			onReasoningEffortChange={onReasoningEffortChange}
			onSelectSession={onSelectSession}
			onToggleRight={onToggleRight}
		/>
	);
});
