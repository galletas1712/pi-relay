export interface TurnDetailRequestToken {
	sessionId: string;
	cardId: string;
	generation: number;
}

export class TurnDetailRequestCoordinator {
	private readonly generations = new Map<string, Map<string, number>>();

	begin(sessionId: string, cardId: string): TurnDetailRequestToken {
		let cards = this.generations.get(sessionId);
		if (!cards) {
			cards = new Map();
			this.generations.set(sessionId, cards);
		}
		const generation = (cards.get(cardId) ?? 0) + 1;
		cards.set(cardId, generation);
		return { sessionId, cardId, generation };
	}

	isCurrent(token: TurnDetailRequestToken): boolean {
		return this.generations.get(token.sessionId)?.get(token.cardId) === token.generation;
	}

	clear(): void {
		this.generations.clear();
	}
}
