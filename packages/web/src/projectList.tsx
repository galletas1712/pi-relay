import { Folder, Plus, SquarePen, X } from "lucide-react";
import { ActionMenu, type ActionMenuItem } from "./actionMenu.tsx";
import { ConnectionBlockedReason } from "./connectionRecovery.tsx";
import { projectTitle } from "./sessionList.ts";
import type { Project } from "./types.ts";

const EMPTY_PROJECT_ACTIVE_SESSION_COUNTS = new Map<string, number>();

export function ProjectList({
	projects,
	projectActiveSessionCounts = EMPTY_PROJECT_ACTIVE_SESSION_COUNTS,
	loading = false,
	fetching = false,
	error = null,
	remoteReadBlockedReason,
	selectedProjectId,
	onRetry,
	onSelectProject,
	onNewProject,
	onEditProject,
	onClose,
}: {
	projects: Project[];
	projectActiveSessionCounts?: ReadonlyMap<string, number>;
	loading?: boolean;
	fetching?: boolean;
	error?: string | null;
	remoteReadBlockedReason?: string | null;
	selectedProjectId: string | null;
	onRetry?: () => void;
	onSelectProject: (projectId: string | null) => void;
	onNewProject: () => void;
	onEditProject: (project: Project) => void;
	onClose?: () => void;
}) {
	return (
		<div className="project-section">
			<div className="project-section-head">
				<span>Projects</span>
				<span className="sidebar-section-actions">
					<button className="icon-button tiny" type="button" onClick={onNewProject} title="new project" aria-label="new project">
						<Plus size={13} />
					</button>
					{onClose ? (
						<button className="plain-close-button sidebar-close" type="button" onClick={onClose} aria-label="close sidebar">
							<X size={14} />
						</button>
					) : null}
				</span>
			</div>
			{error ? (
				<div className="load-error-banner project-load-error" role="alert">
					<div>
						<strong>Couldn’t load projects</strong>
						<span>{error}</span>
					</div>
					{onRetry ? (
						<>
							<button
								type="button"
								className="secondary-button load-error-retry"
								disabled={fetching || !!remoteReadBlockedReason}
								aria-busy={fetching}
								onClick={onRetry}
							>
								{fetching ? "Retrying…" : "Retry"}
							</button>
							<ConnectionBlockedReason reason={remoteReadBlockedReason} />
						</>
					) : null}
				</div>
			) : null}
			<nav aria-label="Projects" aria-busy={loading || fetching}>
				<ul className="project-list">
					<li className={`project-row ${selectedProjectId === null ? "selected" : ""}`}>
						<button
							className="project-row-primary"
							type="button"
							onClick={() => onSelectProject(null)}
							title="Ephemeral host sessions start from your home directory"
							aria-current={selectedProjectId === null ? "page" : undefined}
						>
							<Folder size={14} aria-hidden />
							<span className="project-title">Host</span>
						</button>
					</li>
					{projects.map((project) => {
						const title = projectTitle(project);
						const selected = project.project_id === selectedProjectId;
						const activeSessionCount = projectActiveSessionCounts.get(project.project_id) ?? 0;
						const activeSessionLabel = `${activeSessionCount} active session${activeSessionCount === 1 ? "" : "s"}`;
						return (
							<li className={`project-row ${selected ? "selected" : ""}`} key={project.project_id}>
								<button
									className="project-row-primary"
									type="button"
									onClick={() => onSelectProject(project.project_id)}
									aria-current={selected ? "page" : undefined}
								>
									<Folder size={14} aria-hidden />
									<span className="project-title">{title}</span>
									{activeSessionCount > 0 ? (
										<span
											className="project-active-session-count"
											title={activeSessionLabel}
											aria-label={activeSessionLabel}
										>
											{activeSessionCount}
										</span>
									) : null}
								</button>
								<ActionMenu
									triggerLabel={`Open project actions for ${title}`}
									items={projectMenuItems(project, onEditProject)}
								/>
							</li>
						);
					})}
					{loading && !error ? <li className="empty-list compact">Loading projects…</li> : null}
				</ul>
			</nav>
		</div>
	);
}

export function projectMenuItems(project: Project, onEditProject: (project: Project) => void): ActionMenuItem[] {
	return [
		{
			id: "settings",
			label: "Project settings…",
			icon: <SquarePen size={15} aria-hidden />,
			focusDestination: "dialog",
			onSelect: () => onEditProject(project),
		},
	];
}
