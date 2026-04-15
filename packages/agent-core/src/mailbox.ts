type MailboxPredicate<T> = (item: T) => boolean;

type MailboxWaiter<T> = {
	predicate: MailboxPredicate<T>;
	resolve: (items: T[]) => void;
	reject: (error: unknown) => void;
	signal?: AbortSignal;
	abortHandler?: () => void;
};

function createAbortError(): Error {
	const error = new Error("Mailbox drain aborted");
	error.name = "AbortError";
	return error;
}

export class Mailbox<T> {
	private buffer: T[] = [];
	private waiting?: MailboxWaiter<T>;
	private _closed = false;

	public onEnqueue?: (item: T) => void;

	private removeWaitingAbortListener(waiter: MailboxWaiter<T>): void {
		if (waiter.signal && waiter.abortHandler) {
			waiter.signal.removeEventListener("abort", waiter.abortHandler);
		}
	}

	private extractMatching(predicate: MailboxPredicate<T>): T[] {
		if (this.buffer.length === 0) {
			return [];
		}

		const matching: T[] = [];
		const remaining: T[] = [];

		for (const item of this.buffer) {
			if (predicate(item)) {
				matching.push(item);
				continue;
			}
			remaining.push(item);
		}

		if (matching.length > 0) {
			this.buffer = remaining;
		}

		return matching;
	}

	private flushWaiting(): void {
		if (!this.waiting) {
			return;
		}

		const matching = this.extractMatching(this.waiting.predicate);
		if (matching.length === 0) {
			return;
		}

		const waiter = this.waiting;
		this.waiting = undefined;
		this.removeWaitingAbortListener(waiter);
		waiter.resolve(matching);
	}

	enqueue(item: T): boolean {
		if (this._closed) {
			return false;
		}

		this.buffer.push(item);
		this.flushWaiting();
		this.onEnqueue?.(item);
		return true;
	}

	async drain(predicate: MailboxPredicate<T>, signal?: AbortSignal): Promise<T[]> {
		if (this.waiting) {
			throw new Error("Concurrent drain() calls are not supported");
		}

		const immediate = this.extractMatching(predicate);
		if (immediate.length > 0) {
			return immediate;
		}

		if (this._closed) {
			return [];
		}

		if (signal?.aborted) {
			throw createAbortError();
		}

		return new Promise<T[]>((resolve, reject) => {
			const waiter: MailboxWaiter<T> = {
				predicate,
				resolve: (items) => {
					this.removeWaitingAbortListener(waiter);
					resolve(items);
				},
				reject: (error) => {
					this.removeWaitingAbortListener(waiter);
					reject(error);
				},
				signal,
			};

			if (signal) {
				waiter.abortHandler = () => {
					if (this.waiting !== waiter) {
						return;
					}
					this.waiting = undefined;
					waiter.reject(createAbortError());
				};
				signal.addEventListener("abort", waiter.abortHandler, { once: true });
			}

			this.waiting = waiter;
		});
	}

	tryDrain(predicate: MailboxPredicate<T>): T[] {
		return this.extractMatching(predicate);
	}

	hasMatching(predicate: MailboxPredicate<T>): boolean {
		return this.buffer.some(predicate);
	}

	clear(): void {
		this.buffer = [];
	}

	close(): void {
		if (this._closed) {
			return;
		}

		this._closed = true;

		if (!this.waiting) {
			return;
		}

		const waiter = this.waiting;
		this.waiting = undefined;
		this.removeWaitingAbortListener(waiter);
		waiter.resolve([]);
	}

	get size(): number {
		return this.buffer.length;
	}

	get closed(): boolean {
		return this._closed;
	}
}
