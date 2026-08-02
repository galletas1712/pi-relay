export const FOREGROUND_REFRESH_AFTER_MS = 5 * 60 * 1000;

export function shouldRefreshOnForeground(hiddenAt, now, threshold = FOREGROUND_REFRESH_AFTER_MS) {
	return hiddenAt !== null && now - hiddenAt >= threshold;
}
