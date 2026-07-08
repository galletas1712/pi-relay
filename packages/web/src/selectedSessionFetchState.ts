export interface SelectedSessionFetchState {
	sessionId: string | null;
	selectionVersion: number;
	loading: boolean;
	retrying: boolean;
	hadUsableCache: boolean;
	error: string | null;
}

export class SelectedSessionFetchError extends Error {
	constructor(readonly reason: unknown) {
		super(reason instanceof Error ? reason.message : String(reason));
		this.name = "SelectedSessionFetchError";
	}
}

export function isSelectedSessionFetchError(error: unknown): error is SelectedSessionFetchError {
	return error instanceof SelectedSessionFetchError;
}

export function shouldReportActionError(error: unknown): boolean {
	return !isSelectedSessionFetchError(error);
}

function errorMessage(error: unknown): string {
	return error instanceof Error ? error.message : String(error);
}

export function beginSelectedSessionFetch(
	current: SelectedSessionFetchState,
	sessionId: string,
	selectionVersion: number,
	hasUsableCache: boolean,
): SelectedSessionFetchState {
	const retrying =
		current.sessionId === sessionId &&
		current.selectionVersion === selectionVersion &&
		current.error !== null;
	const usableCache = retrying ? current.hadUsableCache : hasUsableCache;
	return {
		sessionId,
		selectionVersion,
		loading: !usableCache,
		retrying,
		hadUsableCache: usableCache,
		error: retrying ? current.error : null,
	};
}

export function settleSelectedSessionFetch(
	current: SelectedSessionFetchState,
	sessionId: string,
	selectionVersion: number,
	error: string | null,
): SelectedSessionFetchState {
	if (current.sessionId !== sessionId || current.selectionVersion !== selectionVersion) return current;
	return {
		sessionId,
		selectionVersion,
		loading: false,
		retrying: false,
		hadUsableCache: error === null ? true : current.hadUsableCache,
		error,
	};
}

export class SelectedSessionFetchCoordinator {
	private readonly listeners = new Set<() => void>();
	private inFlight: {
		sessionId: string;
		selectionVersion: number;
		request: Promise<unknown>;
	} | null = null;

	constructor(private state: SelectedSessionFetchState) {}

	readonly subscribe = (listener: () => void): (() => void) => {
		this.listeners.add(listener);
		return () => this.listeners.delete(listener);
	};

	readonly getSnapshot = (): SelectedSessionFetchState => this.state;

	select(sessionId: string | null, hasUsableCache: boolean): number {
		const selectionVersion = this.state.selectionVersion + 1;
		this.state = {
			sessionId,
			selectionVersion,
			loading: !!sessionId && !hasUsableCache,
			retrying: false,
			hadUsableCache: hasUsableCache,
			error: null,
		};
		this.publish();
		return selectionVersion;
	}

	restart(hasUsableCache: boolean): number {
		const selectionVersion = this.state.selectionVersion + 1;
		const hadUsableCache = this.state.error ? this.state.hadUsableCache : hasUsableCache;
		this.state = {
			...this.state,
			selectionVersion,
			loading: !!this.state.sessionId && !hadUsableCache,
			retrying: false,
			hadUsableCache,
		};
		this.publish();
		return selectionVersion;
	}

	isCurrent(sessionId: string, selectionVersion: number): boolean {
		return this.state.sessionId === sessionId && this.state.selectionVersion === selectionVersion;
	}

	run<T>(
		sessionId: string,
		hasUsableCache: boolean,
		requestSelectedSession: (selectionVersion: number) => Promise<T>,
	): Promise<T | null> {
		if (this.state.sessionId !== sessionId) return Promise.resolve(null);
		if (
			this.inFlight?.sessionId === sessionId &&
			this.inFlight.selectionVersion === this.state.selectionVersion
		) {
			return this.inFlight.request as Promise<T>;
		}

		const selectionVersion = this.state.selectionVersion;
		this.state = beginSelectedSessionFetch(this.state, sessionId, selectionVersion, hasUsableCache);
		this.publish();

		const request = Promise.resolve()
			.then(() => requestSelectedSession(selectionVersion))
			.then(
				(result) => {
					this.settle(sessionId, selectionVersion, null);
					return result;
				},
				(error) => {
					this.settle(sessionId, selectionVersion, errorMessage(error));
					throw new SelectedSessionFetchError(error);
				},
			)
			.finally(() => {
				if (this.inFlight?.request === request) this.inFlight = null;
			});
		this.inFlight = { sessionId, selectionVersion, request };
		return request;
	}

	private settle(sessionId: string, selectionVersion: number, error: string | null): void {
		const next = settleSelectedSessionFetch(this.state, sessionId, selectionVersion, error);
		if (next === this.state) return;
		this.state = next;
		this.publish();
	}

	private publish(): void {
		for (const listener of this.listeners) listener();
	}
}
