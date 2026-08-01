import { memo, type ReactNode } from "react";
import { CenterViewTabs } from "./centerViewTabs.tsx";
import { GitGraphView } from "./git/gitGraphView.tsx";
import { PullRequestPanel } from "./git/pullRequestPanel.tsx";
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
import type { CenterView, GitPane } from "./workspaceRoute.ts";
import type { GitHubPullRequest } from "./github/githubApi.ts";
import type { GitWorkspaceRepo } from "./github/useGitHubPullRequests.ts";
import type { SessionWorkspace } from "./types.ts";

export interface ChatPaneProps {
	session: SessionDisplayInfo | null;
	snapshot: SessionSnapshot | null;
	centerView: CenterView;
	onCenterViewChange: (view: CenterView) => void;
	entries: TranscriptEntry[];
	turnCards?: TurnCardView[] | null;
	transcriptLoading: boolean;
	transcriptError: string | null;
	transcriptErrorHasUsableCache: boolean;
	transcriptRetrying: boolean;
	hasRunningDelegations: boolean;
	modelOptions: ModelOption[];
	modelValue: string;
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
	transcriptStartContent?: ReactNode;
	loadingTurnId?: string | null;
	hasOlderTurns?: boolean;
	loadingOlderTurns?: boolean;
	onLoadOlderTurns?: (request: OlderTurnsLoadRequest) => Promise<OlderTurnsLoadResult>;
	transcriptDestination?: TranscriptDestination | null;
	transcriptTurnPageIdentity?: TranscriptTurnPageIdentity | null;
	onAcknowledgeTranscriptDestination?: (destinationId: number) => void;
	onRetryTranscript: () => void;
	routeNotice?: ReactNode;
	emptySessionContent?: ReactNode;
	gitPane?: GitPane;
	gitRepo?: string | null;
	selectedPrNumber?: number | null;
	gitRepos?: GitWorkspaceRepo[];
	gitActiveRepo?: GitWorkspaceRepo | null;
	gitSelectedPull?: GitHubPullRequest | null;
	gitActiveRepoPulls?: GitHubPullRequest[];
	gitPullLoading?: boolean;
	sessionWorkspaces?: SessionWorkspace[];
	onSelectGitRepo?: (workspaceDir: string) => void;
	onOpenGitGraph?: () => void;
	onBackFromGitGraph?: () => void;
	inspectorAvailable?: boolean;
}

export const ChatPane = memo(function ChatPane({
	session,
	snapshot,
	centerView,
	onCenterViewChange,
	entries,
	turnCards,
	transcriptLoading,
	transcriptError,
	transcriptErrorHasUsableCache,
	transcriptRetrying,
	hasRunningDelegations,
	modelOptions,
	modelValue,
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
	transcriptStartContent,
	loadingTurnId,
	hasOlderTurns,
	loadingOlderTurns,
	onLoadOlderTurns,
	transcriptDestination,
	transcriptTurnPageIdentity,
	onAcknowledgeTranscriptDestination,
	onRetryTranscript,
	routeNotice,
	emptySessionContent,
	gitPane = "browse",
	gitRepo = null,
	selectedPrNumber = null,
	gitRepos = [],
	gitActiveRepo = null,
	gitSelectedPull = null,
	gitActiveRepoPulls = [],
	gitPullLoading = false,
	sessionWorkspaces = [],
	onSelectGitRepo,
	onOpenGitGraph,
	onBackFromGitGraph,
	inspectorAvailable = true,
}: ChatPaneProps) {
	const loadedLeafId = activeLeafIdFromEntries(entries);
	const visibleActiveLeafId = loadedLeafId ?? snapshot?.active_leaf_id ?? null;
	const gitGraphMode = centerView === "git" && gitPane === "graph";
	const sessionWorkspace =
		gitActiveRepo
			? sessionWorkspaces.find((workspace) => workspace.workspace_dir === gitActiveRepo.workspace.workspace_dir) ??
				null
			: null;

	return (
		<main
			className={`log-pane ${centerView === "git" ? "log-pane-git" : ""} ${gitGraphMode ? "log-pane-git-graph" : ""}`}
			data-slot="agent-log"
		>
			{routeNotice}
			<CenterViewTabs activeView={centerView} onChange={onCenterViewChange} />
			{centerView === "chat" ? (
				<ChatHeader
					session={session}
					snapshot={snapshot}
					hasRunningDelegations={hasRunningDelegations}
					modelOptions={modelOptions}
					modelValue={modelValue}
					modelControlsDisabled={modelControlsDisabled}
					reasoningControlsDisabled={reasoningControlsDisabled}
					mutationBlockedReason={mutationBlockedReason}
					reasoningEfforts={reasoningEfforts}
					reasoningEffort={reasoningEffort}
					rightOpen={rightOpen}
					inspectorAvailable={inspectorAvailable}
					onModelChange={onModelChange}
					onReasoningEffortChange={onReasoningEffortChange}
					onSelectSession={onSelectSession}
					onToggleRight={onToggleRight}
				/>
			) : centerView === "git" ? (
				<div className="git-center-header">
					{gitGraphMode ? (
						<button className="secondary-button" type="button" onClick={onBackFromGitGraph}>
							Back to PR list
						</button>
					) : (
						<p className="git-center-header-title">Pull requests</p>
					)}
				</div>
			) : null}
			{centerView === "chat" ? (
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
					emptySessionContent={emptySessionContent}
					onResumeTurn={onResumeTurn}
					resumingTurnId={resumingTurnId}
					resumeBlockedReason={mutationBlockedReason}
					remoteReadBlockedReason={remoteReadBlockedReason}
					onExpandTurn={onExpandTurn}
					onCollapseTurn={onCollapseTurn}
					transcriptStartContent={transcriptStartContent}
					loadingTurnId={loadingTurnId}
					hasOlderTurns={hasOlderTurns}
					loadingOlderTurns={loadingOlderTurns}
					onLoadOlderTurns={onLoadOlderTurns}
					destination={transcriptDestination}
					turnPageIdentity={transcriptTurnPageIdentity}
					onAcknowledgeDestination={onAcknowledgeTranscriptDestination}
				/>
			) : centerView === "git" ? (
				gitGraphMode ? (
					<div
						className="git-graph-layout"
						role="tabpanel"
						id="center-view-panel-git"
						aria-labelledby="center-view-tab-git"
					>
						<GitGraphView
							repos={gitRepos}
							activeRepo={gitActiveRepo}
							pulls={gitActiveRepoPulls}
							selectedPull={gitSelectedPull}
							onSelectRepo={(workspaceDir) => onSelectGitRepo?.(workspaceDir)}
						/>
						<PullRequestPanel
							repo={gitActiveRepo}
							pull={gitSelectedPull}
							sessionWorkspace={sessionWorkspace}
							loading={gitPullLoading}
						/>
					</div>
				) : (
					<div
						className="center-view-panel git-browse-detail"
						role="tabpanel"
						id="center-view-panel-git"
						aria-labelledby="center-view-tab-git"
					>
						<PullRequestPanel
							repo={gitActiveRepo}
							pull={gitSelectedPull}
							sessionWorkspace={sessionWorkspace}
							loading={gitPullLoading}
							onOpenGraph={onOpenGitGraph}
						/>
					</div>
				)
			) : (
				<div
					className="center-view-panel center-view-panel-placeholder"
					role="tabpanel"
					id="center-view-panel-files"
					aria-labelledby="center-view-tab-files"
				>
					<p className="muted">Filesystem browsing is not available yet.</p>
				</div>
			)}
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
	modelControlsDisabled: boolean;
	reasoningControlsDisabled: boolean;
	mutationBlockedReason?: string | null;
	reasoningEfforts: ReasoningEffort[];
	reasoningEffort: ReasoningEffort;
	rightOpen: boolean;
	inspectorAvailable?: boolean;
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
	modelControlsDisabled,
	reasoningControlsDisabled,
	mutationBlockedReason,
	reasoningEfforts,
	reasoningEffort,
	rightOpen,
	inspectorAvailable = true,
	onModelChange,
	onReasoningEffortChange,
	onSelectSession,
	onToggleRight
}: ChatHeaderProps) {
	const archived = session ? isArchivedSession(session) : false;
	const modelDisabled = modelControlsDisabled || !!mutationBlockedReason;
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
			reasoningDisabled={reasoningControlsDisabled || !!mutationBlockedReason}
			reasoningEfforts={displayedEfforts}
			reasoningEffort={reasoningEffort}
			rightOpen={rightOpen}
			inspectorAvailable={inspectorAvailable}
			onModelChange={onModelChange}
			onReasoningEffortChange={onReasoningEffortChange}
			onSelectSession={onSelectSession}
			onToggleRight={onToggleRight}
		/>
	);
});
