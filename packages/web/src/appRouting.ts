import type { UiSelection } from "./uiResume.ts";
import {
	hostRouteScope,
	messageRecipient,
	projectRouteScope,
	type WorkspaceRoute,
	type WorkspaceRouteHistory,
	type WorkspaceRouteParseResult,
	type WorkspaceRouteUnavailable,
} from "./workspaceRoute.ts";

export function routeScope(projectId: string | null) {
	return projectId === null ? hostRouteScope() : projectRouteScope(projectId);
}

export type RouteValidationState =
	| { kind: "idle" }
	| { kind: "pending" }
	| {
			kind: "valid";
			revision: number;
			canonicalUrl: string;
			projectId: string | null;
			conversationSessionId: string;
		}
	| {
			kind: "unavailable";
			state: WorkspaceRouteUnavailable;
			retryable: boolean;
		};

export function routeScopeProjectId(route: WorkspaceRoute): string | null {
	return route.scope.kind === "project" ? route.scope.projectId : null;
}

export function routeConversationSessionId(route: WorkspaceRoute): string {
	return messageRecipient(route).sessionId;
}

export function routeReadsEnabled(
	result: WorkspaceRouteParseResult,
	validation: RouteValidationState,
	revision: number,
): boolean {
	if (result.kind === "none") return validation.kind === "idle";
	if (result.kind !== "route" || validation.kind !== "valid") return false;
	return (
		validation.revision === revision &&
		validation.canonicalUrl === result.canonicalUrl &&
		validation.projectId === routeScopeProjectId(result.route) &&
		validation.conversationSessionId === routeConversationSessionId(result.route)
	);
}

export function initialRouteResult(history: WorkspaceRouteHistory | null): WorkspaceRouteParseResult {
	return history?.current() ?? { kind: "none" };
}

export function routeInitialSelection(
	result: WorkspaceRouteParseResult,
	legacy: UiSelection,
): { projectId: string | null; conversationSessionId: string | null } {
	if (result.kind === "route") {
		return {
			projectId: routeScopeProjectId(result.route),
			conversationSessionId: routeConversationSessionId(result.route),
		};
	}
	if (result.kind === "unavailable") {
		return { projectId: null, conversationSessionId: null };
	}
	return {
		projectId: legacy.projectId,
		// Legacy identity is not trusted until the selected session's canonical
		// direct parent/root has been resolved.
		conversationSessionId: null,
	};
}

export function projectMismatchUnavailable(
	route: WorkspaceRoute,
	actualProjectId: string | null,
): WorkspaceRouteUnavailable {
	const requestedProject =
		route.scope.kind === "project" ? `project ${route.scope.projectId}` : "Host";
	const actualProject = actualProjectId ? `project ${actualProjectId}` : "Host";
	return {
		kind: "unavailable",
		issue: "project-mismatch",
		message: `This run belongs to ${actualProject}, not ${requestedProject}.`,
		requestedUrl: "",
		backTo: null,
	};
}

export function routeRootUnavailable(message: string): WorkspaceRouteUnavailable {
	return {
		kind: "unavailable",
		issue: "invalid-conversation",
		message,
		requestedUrl: "",
		backTo: null,
	};
}
