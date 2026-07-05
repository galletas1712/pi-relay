import { parseSlash, type ParsedSlash } from "./slash.ts";
import type { SessionSnapshot } from "./types.ts";

export interface ComposerSubmission {
	sessionId: string | null;
	text: string;
	clientControlId: string;
	newSessionId: string;
}

export interface ComposerRoutingDependencies {
	getLoadedSnapshot(sessionId: string): SessionSnapshot | null;
	executeSlash(parsed: ParsedSlash, sessionId: string | null, snapshot: SessionSnapshot | null): Promise<void>;
	queueFollowUp(
		sessionId: string,
		message: string,
		snapshot: SessionSnapshot,
		clientInputId: string,
	): Promise<void>;
	steerSubagent(params: {
		parentSessionId: string;
		subagentSessionId: string;
		message: string;
		clientControlId: string;
	}): Promise<unknown>;
	startNewSession(message: string, clientInputId: string, sessionId: string): Promise<unknown>;
	reportError(error: unknown): void;
}

/** Route one composer submission using Composer's immutable target.
 *
 * Slash commands always win. For ordinary text, a matching loaded snapshot is
 * required before distinguishing a root session from a subagent. App must not
 * reread its current selection here: a selection change after the key/click
 * event may make the captured snapshot unavailable, in which case this fails
 * safely and Composer restores the captured session's draft.
 */
export async function routeComposerSubmission(
	submission: ComposerSubmission,
	dependencies: ComposerRoutingDependencies,
): Promise<boolean> {
	const message = submission.text.trim();
	if (!message) return false;

	try {
		const snapshot = submission.sessionId
			? dependencies.getLoadedSnapshot(submission.sessionId)
			: null;
		const slash = parseSlash(message);
		if (slash) {
			await dependencies.executeSlash(slash, submission.sessionId, snapshot);
			return true;
		}

		if (!submission.sessionId) {
			await dependencies.startNewSession(
				message,
				submission.clientControlId,
				submission.newSessionId,
			);
			return true;
		}

		if (!snapshot || snapshot.session_id !== submission.sessionId) {
			throw new Error("session is still loading");
		}
		if (snapshot.parent_session_id) {
			await dependencies.steerSubagent({
				parentSessionId: snapshot.parent_session_id,
				subagentSessionId: submission.sessionId,
				message,
				clientControlId: submission.clientControlId,
			});
		} else {
			await dependencies.queueFollowUp(
				submission.sessionId,
				message,
				snapshot,
				submission.clientControlId,
			);
		}
		return true;
	} catch (error) {
		dependencies.reportError(error);
		return false;
	}
}
