export interface SessionListRequestState {
	projectId: string | null;
	error: string | null;
	busy: boolean;
}

interface ProjectRequestState<T> {
	generation: number;
	error: string | null;
	request: Promise<T> | null;
	retry: Promise<unknown> | null;
	retryStarting: boolean;
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
		if (!next.request && !next.queryFetching && !next.retry && !next.retryStarting) {
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

		const generation = project.generation;
		project.retryStarting = false;
		const request = Promise.resolve().then(requestCanonicalList);
		project.request = request;
		this.publishIfActive(projectId);

		void request
			.then(
				() => {
					if (!this.matches(projectId, generation, request)) return;
					project.error = null;
				},
				(error) => {
					if (!this.matches(projectId, generation, request)) return;
					project.error = this.messageForError(error);
				},
			)
			.finally(() => {
				if (project.request !== request) return;
				project.request = null;
				if (project.generation === generation) this.publishIfActive(projectId);
			});
		return request;
	}

	retry(projectId: string | null, refetch: () => Promise<unknown>): Promise<unknown> | null {
		const project = this.project(projectId);
		if (projectId !== this.activeProjectId) return null;
		if (project.request) return project.request;
		if (project.retry) return project.retry;

		const generation = project.generation;
		project.retryStarting = true;
		const retry = Promise.resolve().then(refetch);
		project.retry = retry;
		this.publish();
		void retry.then(
			() => this.finishRetry(projectId, generation, retry),
			() => this.finishRetry(projectId, generation, retry),
		);
		return retry;
	}

	private finishRetry(projectId: string | null, generation: number, retry: Promise<unknown>): void {
		const project = this.project(projectId);
		if (project.generation !== generation || project.retry !== retry) return;
		project.retry = null;
		project.retryStarting = false;
		this.publishIfActive(projectId);
	}

	private emptyProjectState(): ProjectRequestState<T> {
		return {
			generation: 0,
			error: null,
			request: null,
			retry: null,
			retryStarting: false,
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

	private matches(projectId: string | null, generation: number, request: Promise<T>): boolean {
		const project = this.project(projectId);
		return project.generation === generation && project.request === request;
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
				project.retryStarting ||
				project.queryFetching,
		};
		for (const listener of this.listeners) listener();
	}
}
