import { memo, type RefObject } from "react";
import { SidebarToolbar } from "./sidebarToolbar.tsx";
import type { SessionListItem } from "./sessionList.ts";
import type { Project } from "./types.ts";
import { ProjectList } from "./projectList.tsx";
import { SessionRow } from "./sessionRow.tsx";
export { ProjectList, projectMenuItems } from "./projectList.tsx";
export { SessionRow, sessionMenuItems } from "./sessionRow.tsx";
export { SidebarToolbar } from "./sidebarToolbar.tsx";
export { LogHeader, NoticeStack } from "./statusPanels.tsx";
export {
	RUN_BOARD_DEFAULT_DELEGATION_COUNT,
	RUN_BOARD_EXPANDED_DELEGATION_COUNT,
	RunBoard,
	RunBoardDelegationList,
	subagentStatusLabel,
} from "./runBoard.tsx";
export { Inspector } from "./inspector.tsx";
export type { InspectorProps } from "./inspector.tsx";

const EMPTY_PROJECT_ACTIVE_SESSION_COUNTS = new Map<string, number>();

export interface SidebarProps {
	projects: Project[];
	projectActiveSessionCounts?: ReadonlyMap<string, number>;
	projectsLoading?: boolean;
	projectsFetching?: boolean;
	projectsError?: string | null;
	selectedProjectId: string | null;
	query: string;
	showArchived: boolean;
	filteredSessions: SessionListItem[];
	selectedId: string | null;
	sessionsLoading?: boolean;
	sessionsFetching?: boolean;
	inert?: boolean;
	newSessionButtonRef?: RefObject<HTMLButtonElement | null>;
	onRetryProjects?: () => void;
	onQueryChange: (query: string) => void;
	onToggleArchived: () => void;
	onNew: () => void;
	onClose?: () => void;
	onSelectProject: (projectId: string | null) => void;
	onNewProject: () => void;
	onEditProject: (project: Project) => void;
	onSelectSession: (sessionId: string) => void;
	onRename: (session: SessionListItem) => void;
	onArchiveToggle: (session: SessionListItem) => void;
	onDelete: (session: SessionListItem) => void;
	mutationBlockedReason?: string | null;
	remoteReadBlockedReason?: string | null;
}

export const Sidebar = memo(function Sidebar({
	projects,
	projectActiveSessionCounts = EMPTY_PROJECT_ACTIVE_SESSION_COUNTS,
	projectsLoading = false,
	projectsFetching = false,
	projectsError = null,
	selectedProjectId,
	query,
	showArchived,
	filteredSessions,
	selectedId,
	sessionsLoading = false,
	sessionsFetching = false,
	inert,
	newSessionButtonRef,
	onRetryProjects,
	onQueryChange,
	onToggleArchived,
	onNew,
	onClose,
	onSelectProject,
	onNewProject,
	onEditProject,
	onSelectSession,
	onRename,
	onArchiveToggle,
	onDelete,
	mutationBlockedReason,
	remoteReadBlockedReason,
}: SidebarProps) {
	return (
		<aside className="sidebar" data-slot="sidebar" inert={inert}>
			<ProjectList
				projects={projects}
				projectActiveSessionCounts={projectActiveSessionCounts}
				loading={projectsLoading}
				fetching={projectsFetching}
				error={projectsError}
				remoteReadBlockedReason={remoteReadBlockedReason}
				selectedProjectId={selectedProjectId}
				onRetry={onRetryProjects}
				onSelectProject={onSelectProject}
				onNewProject={onNewProject}
				onEditProject={onEditProject}
				onClose={onClose}
			/>
			<SidebarToolbar
				disabled={false}
				query={query}
				onQueryChange={onQueryChange}
				showArchived={showArchived}
				onToggleArchived={onToggleArchived}
				onNew={onNew}
				newSessionButtonRef={newSessionButtonRef}
			/>
			<nav className="session-list" aria-label="Sessions" aria-busy={sessionsLoading || sessionsFetching}>
				<ul className="session-list-items">
					{filteredSessions.map((session) => (
						<SessionRow
							key={session.session_id}
							session={session}
							selected={session.session_id === selectedId}
							onSelect={() => onSelectSession(session.session_id)}
							onRename={() => onRename(session)}
							onArchiveToggle={() => onArchiveToggle(session)}
							onDelete={() => onDelete(session)}
							mutationBlockedReason={mutationBlockedReason}
						/>
					))}
					{filteredSessions.length === 0 ? (
						<li className="empty-list">
							{sessionsLoading ? "Loading sessions…" : sessionsFetching ? "Refreshing sessions…" : "No sessions"}
						</li>
					) : null}
				</ul>
			</nav>
		</aside>
	);
});
