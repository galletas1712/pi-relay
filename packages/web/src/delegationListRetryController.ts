export type DelegationListRetryScope = {
	parentSessionId: string | null;
	limit: number;
};

export class DelegationListRetryController {
	private pending = new Map<string | null, Map<number, Promise<unknown>>>();

	retry(
		{ parentSessionId, limit }: DelegationListRetryScope,
		refetch: () => Promise<unknown>,
	): Promise<unknown> {
		const parentPending = this.pending.get(parentSessionId);
		const existing = parentPending?.get(limit);
		if (existing) return existing;
		let request: Promise<unknown>;
		try {
			request = refetch();
		} catch (error) {
			request = Promise.reject(error);
		}
		const pending = request.finally(() => {
			const currentParentPending = this.pending.get(parentSessionId);
			if (currentParentPending?.get(limit) !== pending) return;
			currentParentPending.delete(limit);
			if (currentParentPending.size === 0) this.pending.delete(parentSessionId);
		});
		if (parentPending) {
			parentPending.set(limit, pending);
		} else {
			this.pending.set(parentSessionId, new Map([[limit, pending]]));
		}
		return pending;
	}

	isPending({ parentSessionId, limit }: DelegationListRetryScope): boolean {
		return this.pending.get(parentSessionId)?.has(limit) ?? false;
	}
}
