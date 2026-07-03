export interface StopSessionDependencies {
	interrupt(sessionId: string): Promise<unknown>;
	refresh(sessionId: string): Promise<unknown>;
	invalidateSessions(): Promise<unknown>;
}

/** Stop one immutable session target captured by the caller.
 *
 * This intentionally has no selection getter: changing selection while the
 * interrupt is in flight cannot retarget the request or its refresh.
 */
export async function stopSession(
	sessionId: string,
	dependencies: StopSessionDependencies,
): Promise<void> {
	await dependencies.interrupt(sessionId);
	await Promise.all([
		dependencies.refresh(sessionId),
		dependencies.invalidateSessions(),
	]);
}
