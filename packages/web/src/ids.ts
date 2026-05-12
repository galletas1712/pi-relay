export function randomId(prefix: string): string {
	if (typeof crypto !== "undefined" && "randomUUID" in crypto) {
		return `${prefix}_${crypto.randomUUID()}`;
	}
	return `${prefix}_${Date.now()}_${Math.random().toString(16).slice(2)}`;
}
