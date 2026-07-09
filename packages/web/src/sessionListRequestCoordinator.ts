export interface SessionListRequestState {
	projectId: string | null;
	error: string | null;
	busy: boolean;
}

interface ProjectRequestState<T> {
	error: string | null;
	request: Promise<T> | null;
	retry: Promise<unknown> | null;
	queryFetching: boolean;
}

function defaultErrorMessage(error: unknown): string {
	return error instanceof Error ? error.message : String(error);
}

export class SessionListRequestCoordinator<T> {
	private activeProjectId: string | null;
	private readonly projects = new Map<string | null, ProjectRequestState<T>>();
	private readonly listeners = new Set<() => void>();
	private snapshot: SessionListRequestState;

	constructor(
		projectId: string | null,
		private readonly messageForError: (error: unknown) => string = defaultErrorMessage,
	) {
		this.activeProjectId = projectId;
		this.projects.set(projectId, this.emptyProjectState());
		this.snapshot = { projectId, error: null, busy: false };
	}

	readonly subscribe = (listener: () => void): (() => void) => {
		this.listeners.add(listener);
		return () => this.listeners.delete(listener);
	};

	readonly getSnapshot = (): SessionListRequestState => this.snapshot;

	selectProject(projectId: string | null): void {
		if (projectId === this.activeProjectId) return;
		this.activeProjectId = projectId;
		const next = this.project(projectId);
		if (!next.request && !next.queryFetching && !next.retry) {
			next.error = null;
		}
		this.publish();
	}

	setQueryFetching(projectId: string | null, queryFetching: boolean): void {
		const project = this.project(projectId);
		if (project.queryFetching === queryFetching) return;
		project.queryFetching = queryFetching;
		this.publishIfActive(projectId);
	}

	run(projectId: string | null, requestCanonicalList: () => Promise<T>): Promise<T> {
		const project = this.project(projectId);
		if (project.request) return project.request;

		const request = Promise.resolve().then(requestCanonicalList);
		project.request = request;
		this.publishIfActive(projectId);

		void request
			.then(
				() => {
					if (project.request !== request) return;
					project.error = null;
				},
				(error) => {
					if (project.request !== request) return;
					project.error = this.messageForError(error);
				},
			)
			.finally(() => {
				if (project.request !== request) return;
				project.request = null;
				this.publishIfActive(projectId);
			});
		return request;
	}

	retry(projectId: string | null, refetch: () => Promise<unknown>): Promise<unknown> | null {
		const project = this.project(projectId);
		if (projectId !== this.activeProjectId) return null;
		if (project.request) return project.request;
		if (project.retry) return project.retry;

		const retry = Promise.resolve().then(refetch);
		project.retry = retry;
		this.publish();
		void retry.then(
			() => this.finishRetry(projectId, retry),
			() => this.finishRetry(projectId, retry),
		);
		return retry;
	}

	private finishRetry(projectId: string | null, retry: Promise<unknown>): void {
		const project = this.project(projectId);
		if (project.retry !== retry) return;
		project.retry = null;
		this.publishIfActive(projectId);
	}

	private emptyProjectState(): ProjectRequestState<T> {
		return {
			error: null,
			request: null,
			retry: null,
			queryFetching: false,
		};
	}

	private project(projectId: string | null): ProjectRequestState<T> {
		let project = this.projects.get(projectId);
		if (!project) {
			project = this.emptyProjectState();
			this.projects.set(projectId, project);
		}
		return project;
	}

	private publishIfActive(projectId: string | null): void {
		if (projectId === this.activeProjectId) this.publish();
	}

	private publish(): void {
		const project = this.project(this.activeProjectId);
		this.snapshot = {
			projectId: this.activeProjectId,
			error: project.error,
			busy:
				project.request !== null ||
				project.retry !== null ||
				project.queryFetching,
		};
		for (const listener of this.listeners) listener();
	}
}
