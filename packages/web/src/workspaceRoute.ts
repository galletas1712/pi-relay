import type { UiSelection } from "./uiResume.ts";

export type ExecutionView = "overview" | "activity" | "handoffs";

declare const routeIdBrand: unique symbol;

/**
 * IDs in typed route state have passed the same validation used by the URL
 * parser. Exported builders are the supported way to obtain them and throw a
 * TypeError for programmer-invalid input; URL parsing remains non-throwing and
 * returns an owned unavailable result.
 */
export type RouteId = string & { readonly [routeIdBrand]: true };

export type WorkspaceRouteScope =
	| { readonly kind: "project"; readonly projectId: RouteId }
	| { readonly kind: "host" };

/**
 * Root identity is derived from WorkspaceRoute.rootSessionId. It is
 * intentionally impossible for a root reference to carry a second ID.
 * Membership/availability belongs to the integration result, not URL identity.
 */
export type RouteConversation =
	| { readonly kind: "root" }
	| { readonly kind: "agent"; readonly sessionId: RouteId };

export type ExecutionFocus =
	| { readonly kind: "root" }
	| {
			readonly kind: "delegation";
			readonly delegationId: RouteId;
		}
	| {
			readonly kind: "agent";
			readonly sessionId: RouteId;
		};

/**
 * The handoff contract does not yet define a composite URL grammar. Keep its
 * server-owned reference opaque, but typed and syntax-checked at the route edge.
 */
export interface HandoffReference {
	readonly kind: "handoff";
	readonly ref: RouteId;
}

interface WorkspaceRouteBase {
	readonly scope: WorkspaceRouteScope;
	readonly rootSessionId: RouteId;
}

export interface ConversationRoute extends WorkspaceRouteBase {
	readonly destination: "conversation";
	readonly conversation: RouteConversation;
}

export interface ExecutionRoute extends WorkspaceRouteBase {
	readonly destination: "execution";
	readonly view: ExecutionView;
	readonly conversation: RouteConversation;
	readonly focus: ExecutionFocus;
	readonly handoff: HandoffReference | null;
}

export type WorkspaceRoute = ConversationRoute | ExecutionRoute;

/** A derived message target, never independently writable route state. */
export type MessageRecipient =
	| { readonly kind: "root"; readonly sessionId: RouteId }
	| { readonly kind: "agent"; readonly sessionId: RouteId };

export type WorkspaceRouteWarning =
	| {
			kind: "invalid-execution-conversation";
			persistent: true;
			reason: "malformed" | "wrong-root" | "unavailable" | "wrong-root-membership";
			requestedValue: string | null;
			message: string;
		}
	| {
			kind: "unsupported-query";
			persistent: false;
			parameters: string[];
			message: string;
		}
	| {
			kind: "unsupported-fragment";
			persistent: false;
			message: string;
		};

export type RouteCorrectionReason =
	| "explicit-conversation-default"
	| "explicit-focus-default"
	| "explicit-overview-default"
	| "invalid-conversation-fallback"
	| "unsupported-query"
	| "unsupported-fragment"
	| "noncanonical-url";

export interface WorkspaceRouteCorrection {
	kind: "replace";
	url: string;
	reasons: RouteCorrectionReason[];
}

export interface WorkspaceRouteMatch {
	kind: "route";
	route: WorkspaceRoute;
	canonicalUrl: string;
	warnings: WorkspaceRouteWarning[];
	correction: WorkspaceRouteCorrection | null;
}

export interface WorkspaceRouteRecovery {
	label: "root-conversation" | "root-outline";
	url: string;
}

export type WorkspaceRouteUnavailableIssue =
	| "invalid-path"
	| "invalid-path-encoding"
	| "invalid-query-encoding"
	| "invalid-conversation"
	| "project-mismatch"
	| "invalid-focus"
	| "invalid-handoff"
	| "unsupported-overview";

export interface WorkspaceRouteUnavailable {
	kind: "unavailable";
	issue: WorkspaceRouteUnavailableIssue;
	message: string;
	requestedUrl: string;
	backTo: WorkspaceRouteRecovery | null;
}

export interface NoWorkspaceRoute {
	kind: "none";
}

export type WorkspaceRouteParseResult =
	| NoWorkspaceRoute
	| WorkspaceRouteMatch
	| WorkspaceRouteUnavailable;

export interface WorkspaceRouteLocation {
	pathname: string;
	search?: string;
	hash?: string;
}

interface ParsedLocation {
	pathname: string;
	search: string;
	hash: string;
	requestedUrl: string;
}

interface ParsedPathBase {
	scope: WorkspaceRouteScope;
	rootSessionId: RouteId;
	suffixOffset: number;
}

interface ParsedQuery {
	values: Map<string, string[]>;
}

const EXECUTION_VIEWS = new Set<ExecutionView>(["overview", "activity", "handoffs"]);
const SUPPORTED_QUERY_PARAMETERS = new Set(["conversation", "focus", "handoff", "overview"]);
const INVALID_ID_CHARACTERS = /[\/\\\u0000-\u001f\u007f-\u009f]/u;

/**
 * Parse only the `/w` namespace. Non-workspace paths are left alone; malformed
 * paths within that namespace are owned unavailable states rather than throws.
 */
export function parseWorkspaceRoute(input: string | WorkspaceRouteLocation): WorkspaceRouteParseResult {
	const location = locationParts(input);
	if (!ownsWorkspacePath(location.pathname)) return { kind: "none" };

	const rawSegments = location.pathname.split("/");
	const decodedSegments = decodePathSegments(rawSegments);
	if (!decodedSegments) {
		return unavailable(
			"invalid-path-encoding",
			"The workspace URL contains invalid path encoding.",
			location,
			recoveryFromPartiallyDecodablePath(rawSegments),
		);
	}

	const base = parsePathBase(rawSegments, decodedSegments);
	if (!base) {
		return unavailable("invalid-path", "The workspace URL does not match a supported project or Host route.", location, null);
	}

	const rootConversationRecovery = recoveryForRootConversation(base);
	const suffix = rawSegments.slice(base.suffixOffset);
	if (suffix[0] === "conversation") {
		if (suffix.length !== 2) {
			return unavailable(
				"invalid-conversation",
				"The Conversation URL must contain exactly one conversation session ID.",
				location,
				rootConversationRecovery,
			);
		}
		const conversationSessionId = routeId(decodedSegments[base.suffixOffset + 1]);
		if (!conversationSessionId) {
			return unavailable(
				"invalid-conversation",
				"The requested conversation session ID is malformed.",
				location,
				rootConversationRecovery,
			);
		}

		const query = parseQuery(location.search);
		if (!query) {
			return unavailable(
				"invalid-query-encoding",
				"The workspace URL contains invalid query encoding.",
				location,
				rootConversationRecovery,
			);
		}
		const route: ConversationRoute = {
			destination: "conversation",
			scope: base.scope,
			rootSessionId: base.rootSessionId,
			conversation:
				conversationSessionId === base.rootSessionId
					? rootConversation()
					: { kind: "agent", sessionId: conversationSessionId },
		};
		return matchedRoute(route, location, unsupportedQueryWarnings(query, location.hash, new Set()), []);
	}

	if (suffix[0] !== "execution" || suffix.length !== 2 || !EXECUTION_VIEWS.has(suffix[1] as ExecutionView)) {
		return unavailable(
			"invalid-path",
			"The workspace URL does not match a supported Conversation or Execution destination.",
			location,
			rootConversationRecovery,
		);
	}

	const query = parseQuery(location.search);
	if (!query) {
		return unavailable(
			"invalid-query-encoding",
			"The workspace URL contains invalid query encoding.",
			location,
			rootOutlineRecovery(base),
		);
	}
	return parseExecutionRoute(base, suffix[1] as ExecutionView, query, location);
}

function parseExecutionRoute(
	base: ParsedPathBase,
	view: ExecutionView,
	query: ParsedQuery,
	location: ParsedLocation,
): WorkspaceRouteParseResult {
	const warnings = unsupportedQueryWarnings(query, location.hash, SUPPORTED_QUERY_PARAMETERS);
	const correctionReasons: RouteCorrectionReason[] = [];

	const conversationValues = query.values.get("conversation") ?? [];
	let conversation: RouteConversation;
	if (conversationValues.length === 0) {
		conversation = rootConversation();
	} else {
		const requested = conversationValues.length === 1 ? conversationValues[0] : null;
		const parsed = requested === null ? null : parseConversationReference(requested, base.rootSessionId);
		if (!parsed || parsed.kind === "wrong-root") {
			conversation = rootConversation();
			warnings.unshift(invalidConversationWarning(requested, parsed?.kind === "wrong-root" ? "wrong-root" : "malformed"));
			correctionReasons.push("invalid-conversation-fallback");
		} else if (parsed.kind === "root") {
			conversation = rootConversation();
			correctionReasons.push("explicit-conversation-default");
		} else {
			conversation = {
				kind: "agent",
				sessionId: parsed.sessionId,
			};
		}
	}

	const focusValues = query.values.get("focus") ?? [];
	let focus: ExecutionFocus;
	if (focusValues.length === 0) {
		focus = rootFocus();
	} else if (focusValues.length !== 1) {
		return unavailable(
			"invalid-focus",
			"The Execution focus query must contain exactly one typed reference.",
			location,
			rootOutlineRecovery(base),
		);
	} else {
		const parsed = parseFocusReference(focusValues[0], base.rootSessionId);
		if (!parsed) {
			return unavailable(
				"invalid-focus",
				"The requested Execution focus is malformed or belongs to another root.",
				location,
				rootOutlineRecovery(base),
			);
		}
		focus = parsed;
		if (focus.kind === "root") correctionReasons.push("explicit-focus-default");
	}

	const handoffValues = query.values.get("handoff") ?? [];
	let handoff: HandoffReference | null = null;
	const parsedHandoff = handoffValues.length === 1 ? routeId(handoffValues[0]) : null;
	if (handoffValues.length > 1 || (handoffValues.length === 1 && !parsedHandoff)) {
		return unavailable(
			"invalid-handoff",
			"The requested handoff reference is malformed.",
			location,
			rootOutlineRecovery(base),
		);
	}
	if (parsedHandoff) {
		handoff = {
			kind: "handoff",
			ref: parsedHandoff,
		};
	}

	const overviewValues = query.values.get("overview") ?? [];
	if (overviewValues.length > 0) {
		if (overviewValues.length === 1 && overviewValues[0] === "outline" && view === "overview") {
			correctionReasons.push("explicit-overview-default");
		} else {
			return unavailable(
				"unsupported-overview",
				overviewValues.includes("map")
					? "Execution Map is not available yet. Use the root Outline instead."
					: "The requested Execution overview mode is not supported.",
				location,
				rootOutlineRecovery(base),
			);
		}
	}

	const route: ExecutionRoute = {
		destination: "execution",
		scope: base.scope,
		rootSessionId: base.rootSessionId,
		view,
		conversation,
		focus,
		handoff,
	};
	return matchedRoute(route, location, warnings, correctionReasons);
}

/**
 * Return a canonical path/query string. Defaults are represented in the typed
 * route but intentionally omitted from the URL.
 */
export function serializeWorkspaceRoute(route: WorkspaceRoute): string {
	assertWorkspaceRoute(route);
	const prefix =
		route.scope.kind === "project"
			? `/w/project/${encodePart(route.scope.projectId)}/run/${encodePart(route.rootSessionId)}`
			: `/w/host/run/${encodePart(route.rootSessionId)}`;
	if (route.destination === "conversation") {
		const conversationSessionId =
			route.conversation.kind === "root" ? route.rootSessionId : route.conversation.sessionId;
		return `${prefix}/conversation/${encodePart(conversationSessionId)}`;
	}

	const parameters: [string, string][] = [];
	if (route.conversation.kind === "agent") {
		parameters.push(["conversation", `agent:${route.conversation.sessionId}`]);
	}
	if (route.focus.kind === "delegation") {
		parameters.push(["focus", `delegation:${route.focus.delegationId}`]);
	} else if (route.focus.kind === "agent") {
		parameters.push(["focus", `agent:${route.focus.sessionId}`]);
	}
	if (route.handoff) parameters.push(["handoff", route.handoff.ref]);
	const search = parameters.length
		? `?${parameters.map(([name, value]) => `${name}=${encodePart(value)}`).join("&")}`
		: "";
	return `${prefix}/execution/${route.view}${search}`;
}

/** The sole source for a message recipient: the route's effective conversation. */
export function messageRecipient(route: WorkspaceRoute): MessageRecipient {
	return route.conversation.kind === "root"
		? { kind: "root", sessionId: route.rootSessionId }
		: { kind: "agent", sessionId: route.conversation.sessionId };
}

export function projectRouteScope(projectId: string): WorkspaceRouteScope {
	return { kind: "project", projectId: requireRouteId(projectId, "project ID") };
}

export function hostRouteScope(): WorkspaceRouteScope {
	return { kind: "host" };
}

export function rootConversationRoute(scope: WorkspaceRouteScope, rootSessionId: string): ConversationRoute {
	assertRouteScope(scope);
	return {
		destination: "conversation",
		scope,
		rootSessionId: requireRouteId(rootSessionId, "root session ID"),
		conversation: rootConversation(),
	};
}

/**
 * Convert a server-discovered membership failure for a required Conversation
 * path into an owned Unavailable state. Required conversation failures are
 * never rewritten to another transcript.
 */
export function unavailableConversationRoute(
	route: ConversationRoute,
	message = "The requested conversation is unavailable for this root run.",
): WorkspaceRouteUnavailable {
	return {
		kind: "unavailable",
		issue: "invalid-conversation",
		message,
		requestedUrl: serializeWorkspaceRoute(route),
		backTo: {
			label: "root-conversation",
			url: serializeWorkspaceRoute(rootConversationRoute(route.scope, route.rootSessionId)),
		},
	};
}

/**
 * Convert server-discovered focus/handoff availability failures into an owned
 * detail state. The selected reference is not silently changed.
 */
export function unavailableExecutionDetail(
	route: ExecutionRoute,
	detail: "focus" | "handoff",
	message = detail === "focus"
		? "The requested Execution focus is unavailable."
		: "The requested handoff is unavailable.",
): WorkspaceRouteUnavailable {
	return {
		kind: "unavailable",
		issue: detail === "focus" ? "invalid-focus" : "invalid-handoff",
		message,
		requestedUrl: serializeWorkspaceRoute(route),
		backTo: {
			label: "root-outline",
			url: serializeWorkspaceRoute({
				...route,
				view: "overview",
				conversation: rootConversation(),
				focus: rootFocus(),
				handoff: null,
			}),
		},
	};
}

export function agentFocus(sessionId: string): ExecutionFocus {
	return { kind: "agent", sessionId: requireRouteId(sessionId, "agent session ID") };
}

export function delegationFocus(delegationId: string): ExecutionFocus {
	return { kind: "delegation", delegationId: requireRouteId(delegationId, "delegation ID") };
}

export function handoffReference(ref: string): HandoffReference {
	return { kind: "handoff", ref: requireRouteId(ref, "handoff reference") };
}

export interface RouteNavigation {
	kind: "route";
	history: "push" | "replace";
	route: WorkspaceRoute;
	url: string;
	action:
		| "root-selection"
		| "destination"
		| "execution-view"
		| "focus"
		| "agent-conversation"
		| "message-agent"
		| "handoff-detail";
	focusComposer?: true;
	handoffParentUrl?: string;
}

export interface CloseHandoffNavigation {
	kind: "close-handoff";
	route: ExecutionRoute;
	url: string;
}

export type WorkspaceNavigation = RouteNavigation | CloseHandoffNavigation;

/** Selecting a run always enters its root Conversation and pushes history. */
export function selectRootRun(scope: WorkspaceRouteScope, rootSessionId: string): RouteNavigation {
	return navigation("push", rootConversationRoute(scope, rootSessionId), "root-selection");
}

/** Conversation and Execution destination links preserve the effective conversation. */
export function showConversation(route: WorkspaceRoute): RouteNavigation {
	const conversation = messageRecipient(route);
	const next: ConversationRoute = {
		destination: "conversation",
		scope: route.scope,
		rootSessionId: route.rootSessionId,
		conversation:
			conversation.kind === "root"
				? rootConversation()
				: { kind: "agent", sessionId: conversation.sessionId },
	};
	return navigation("push", next, "destination");
}

export function showExecution(route: WorkspaceRoute, view: ExecutionView = "overview"): RouteNavigation {
	const next = executionRouteFor(route, view);
	return navigation("push", next, "destination");
}

/** Execution subview links are major destinations and therefore push. */
export function changeExecutionView(route: ExecutionRoute, view: ExecutionView): RouteNavigation {
	const next: ExecutionRoute = {
		...route,
		view,
		handoff: view === route.view ? route.handoff : null,
	};
	return navigation("push", next, "execution-view");
}

/** Keyboard/click focus roving changes only focus and replaces the current entry. */
export function changeExecutionFocus(route: ExecutionRoute, focus: ExecutionFocus): RouteNavigation {
	return navigation("replace", { ...route, focus }, "focus");
}

/**
 * Run Navigator links push when entering/changing a destination, but replace
 * when focus is the only changed route dimension.
 */
export function navigateExecutionFocus(
	route: WorkspaceRoute,
	view: ExecutionView,
	focus: ExecutionFocus,
): RouteNavigation {
	const base = executionRouteFor(route, view);
	const next: ExecutionRoute = { ...base, focus };
	if (route.destination === "execution" && route.view === view) {
		return navigation("replace", { ...next, handoff: route.handoff }, "focus");
	}
	return navigation("push", next, "execution-view");
}

export function openAgentConversation(route: WorkspaceRoute, sessionId: string): RouteNavigation {
	return agentConversationNavigation(route, sessionId, "agent-conversation");
}

export function messageAgent(route: WorkspaceRoute, sessionId: string): RouteNavigation {
	return {
		...agentConversationNavigation(route, sessionId, "message-agent"),
		focusComposer: true,
	};
}

/**
 * Durable handoff details preserve conversation/focus and push so ordinary
 * Close can use Back. If focus is subsequently replaced while the detail is
 * open, that replacement intentionally drops this entry's parent marker;
 * Close then replaces to the new focus instead of going Back to stale focus.
 */
export function openHandoff(route: ExecutionRoute, handoff: HandoffReference): RouteNavigation {
	const next: ExecutionRoute = { ...route, handoff };
	const result = navigation("push", next, "handoff-detail");
	return {
		...result,
		handoffParentUrl: serializeWorkspaceRoute({ ...route, handoff: null }),
	};
}

/**
 * The history adapter uses Back for a detail opened by this adapter. A direct
 * handoff deep link has no parent marker, so Close safely replaces it in place.
 */
export function closeHandoff(route: ExecutionRoute): CloseHandoffNavigation {
	const next: ExecutionRoute = { ...route, handoff: null };
	return { kind: "close-handoff", route: next, url: serializeWorkspaceRoute(next) };
}

/**
 * Convert a membership failure discovered by the integration layer into the
 * same visible root fallback/canonical replacement used for malformed query
 * input. The parser intentionally leaves membership to the integration layer.
 */
export function fallbackExecutionConversation(
	route: ExecutionRoute,
	reason: "unavailable" | "wrong-root-membership",
): WorkspaceRouteMatch {
	const requestedValue =
		route.conversation.kind === "agent" ? `agent:${route.conversation.sessionId}` : `root:${route.rootSessionId}`;
	const corrected: ExecutionRoute = {
		...route,
		conversation: rootConversation(),
	};
	const canonicalUrl = serializeWorkspaceRoute(corrected);
	return {
		kind: "route",
		route: corrected,
		canonicalUrl,
		warnings: [invalidConversationWarning(requestedValue, reason)],
		correction: {
			kind: "replace",
			url: canonicalUrl,
			reasons: ["invalid-conversation-fallback"],
		},
	};
}

export interface WorkspaceHistoryLike {
	readonly state?: unknown;
	pushState(data: unknown, unused: string, url?: string | URL | null): void;
	replaceState(data: unknown, unused: string, url?: string | URL | null): void;
	back?(): void;
}

export interface WorkspacePopstateSource {
	addEventListener(type: "popstate", listener: EventListener): void;
	removeEventListener(type: "popstate", listener: EventListener): void;
}

export interface WorkspaceRouteHistoryDependencies {
	history: WorkspaceHistoryLike;
	location: WorkspaceRouteLocation;
	events: WorkspacePopstateSource;
}

interface WorkspaceHistoryMarker {
	piRelayWorkspaceRoute: {
		version: 1;
		action: RouteNavigation["action"];
		handoffParentUrl?: string;
	};
}

/**
 * Minimal history adapter. Dependencies are injected, and no browser global is
 * read until the optional browser factory is called.
 */
export class WorkspaceRouteHistory {
	constructor(private readonly dependencies: WorkspaceRouteHistoryDependencies) {}

	current(): WorkspaceRouteParseResult {
		return parseWorkspaceRoute(this.dependencies.location);
	}

	apply(navigation: WorkspaceNavigation): WorkspaceRouteParseResult | null {
		if (navigation.kind === "close-handoff") return this.closeHandoff(navigation);
		const marker: WorkspaceHistoryMarker = {
			piRelayWorkspaceRoute: {
				version: 1,
				action: navigation.action,
				handoffParentUrl: navigation.handoffParentUrl,
			},
		};
		if (navigation.history === "push") {
			this.dependencies.history.pushState(marker, "", navigation.url);
		} else {
			this.dependencies.history.replaceState(marker, "", navigation.url);
		}
		return parseWorkspaceRoute(navigation.url);
	}

	/** Apply a parser-requested correction without adding a Back entry. */
	correct(result: WorkspaceRouteParseResult): WorkspaceRouteParseResult {
		if (result.kind !== "route" || !result.correction) return result;
		this.dependencies.history.replaceState(this.dependencies.history.state ?? null, "", result.correction.url);
		return { ...result, correction: null };
	}

	/**
	 * New-session drafts intentionally have no run route. Clearing the owned
	 * namespace keeps refresh URL-first without inventing an unstarted run.
	 */
	clear(history: "push" | "replace" = "push"): NoWorkspaceRoute {
		if (history === "push") {
			this.dependencies.history.pushState(this.dependencies.history.state ?? null, "", "/");
		} else {
			this.dependencies.history.replaceState(this.dependencies.history.state ?? null, "", "/");
		}
		return { kind: "none" };
	}

	/**
	 * Popstate emits one complete parsed snapshot. It never pushes, replaces, or
	 * dispatches application mutations.
	 */
	subscribe(listener: (result: WorkspaceRouteParseResult) => void): () => void {
		const onPopstate: EventListener = () => listener(this.current());
		this.dependencies.events.addEventListener("popstate", onPopstate);
		return () => this.dependencies.events.removeEventListener("popstate", onPopstate);
	}

	private closeHandoff(navigation: CloseHandoffNavigation): WorkspaceRouteParseResult | null {
		const state = workspaceHistoryMarker(this.dependencies.history.state);
		if (
			state?.action === "handoff-detail" &&
			state.handoffParentUrl === navigation.url &&
			this.dependencies.history.back
		) {
			this.dependencies.history.back();
			return null;
		}
		this.dependencies.history.replaceState(this.dependencies.history.state ?? null, "", navigation.url);
		return parseWorkspaceRoute(navigation.url);
	}
}

/** Browser convenience factory; safe to import during SSR/static rendering. */
export function browserWorkspaceRouteHistory(): WorkspaceRouteHistory | null {
	if (typeof window === "undefined") return null;
	return new WorkspaceRouteHistory({
		history: window.history,
		location: window.location,
		events: window,
	});
}

export type LegacyRootResolution =
	| { kind: "known"; rootSessionId: string }
	| { kind: "unresolved" }
	| { kind: "failed" };

export type LegacyWorkspaceResume =
	| { kind: "url"; result: WorkspaceRouteMatch | WorkspaceRouteUnavailable }
	| { kind: "legacy-route"; navigation: RouteNavigation }
	| {
			kind: "needs-root-resolution";
			scope: WorkspaceRouteScope;
			selectedSessionId: string;
		}
	| {
			kind: "root-resolution-failed";
			scope: WorkspaceRouteScope;
			selectedSessionId: string;
		}
	| { kind: "empty" };

/**
 * One-time URL-first migration seam. A legacy selected session is never assumed
 * to be a root; App must resolve its parent/root before constructing a route.
 */
export function legacyWorkspaceResume(
	parsed: WorkspaceRouteParseResult,
	legacySelection: UiSelection,
	rootResolution: LegacyRootResolution = { kind: "unresolved" },
): LegacyWorkspaceResume {
	if (parsed.kind !== "none") return { kind: "url", result: parsed };
	if (!legacySelection.sessionId) return { kind: "empty" };
	const scope = legacySelection.projectId === null ? hostRouteScope() : projectRouteScope(legacySelection.projectId);
	const selectedSessionId = requireRouteId(legacySelection.sessionId, "legacy selected session ID");
	if (rootResolution.kind === "unresolved") {
		return {
			kind: "needs-root-resolution",
			scope,
			selectedSessionId,
		};
	}
	if (rootResolution.kind === "failed") {
		return {
			kind: "root-resolution-failed",
			scope,
			selectedSessionId,
		};
	}
	const rootSessionId = requireRouteId(rootResolution.rootSessionId, "resolved root session ID");
	const route: ConversationRoute = {
		...rootConversationRoute(scope, rootSessionId),
		conversation:
			selectedSessionId === rootSessionId
				? rootConversation()
				: { kind: "agent", sessionId: selectedSessionId },
	};
	return {
		kind: "legacy-route",
		navigation: navigation("replace", route, "root-selection"),
	};
}

function matchedRoute(
	route: WorkspaceRoute,
	location: ParsedLocation,
	warnings: WorkspaceRouteWarning[],
	reasons: RouteCorrectionReason[],
): WorkspaceRouteMatch {
	const canonicalUrl = serializeWorkspaceRoute(route);
	const correctionReasons = [...reasons];
	for (const warning of warnings) {
		if (warning.kind === "unsupported-query") correctionReasons.push("unsupported-query");
		if (warning.kind === "unsupported-fragment") correctionReasons.push("unsupported-fragment");
	}
	if (location.requestedUrl !== canonicalUrl && correctionReasons.length === 0) {
		correctionReasons.push("noncanonical-url");
	}
	return {
		kind: "route",
		route,
		canonicalUrl,
		warnings,
		correction:
			location.requestedUrl === canonicalUrl
				? null
				: {
					kind: "replace",
					url: canonicalUrl,
					reasons: unique(correctionReasons),
				},
	};
}

function parsePathBase(raw: string[], decoded: string[]): ParsedPathBase | null {
	if (raw[0] !== "" || raw[1] !== "w") return null;
	const projectId = routeId(decoded[3]);
	const projectRootSessionId = routeId(decoded[5]);
	if (raw[2] === "project" && raw[4] === "run" && projectId && projectRootSessionId) {
		return {
			scope: { kind: "project", projectId },
			rootSessionId: projectRootSessionId,
			suffixOffset: 6,
		};
	}
	const hostRootSessionId = routeId(decoded[4]);
	if (raw[2] === "host" && raw[3] === "run" && hostRootSessionId) {
		return {
			scope: { kind: "host" },
			rootSessionId: hostRootSessionId,
			suffixOffset: 5,
		};
	}
	return null;
}

function parseConversationReference(
	value: string,
	rootSessionId: RouteId,
):
	| { kind: "root" }
	| { kind: "agent"; sessionId: RouteId }
	| { kind: "wrong-root" }
	| null {
	const typed = splitTypedReference(value);
	const id = routeId(typed?.id);
	if (!typed || !id) return null;
	if (typed.kind === "root") {
		return id === rootSessionId ? { kind: "root" } : { kind: "wrong-root" };
	}
	if (typed.kind === "agent" && id !== rootSessionId) {
		return { kind: "agent", sessionId: id };
	}
	return null;
}

function parseFocusReference(value: string, rootSessionId: RouteId): ExecutionFocus | null {
	const typed = splitTypedReference(value);
	const id = routeId(typed?.id);
	if (!typed || !id) return null;
	if (typed.kind === "root") {
		return id === rootSessionId ? rootFocus() : null;
	}
	if (typed.kind === "delegation") {
		return {
			kind: "delegation",
			delegationId: id,
		};
	}
	if (typed.kind === "agent" && id !== rootSessionId) {
		return {
			kind: "agent",
			sessionId: id,
		};
	}
	return null;
}

function splitTypedReference(value: string): { kind: string; id: string } | null {
	const separator = value.indexOf(":");
	if (separator < 1) return null;
	return { kind: value.slice(0, separator), id: value.slice(separator + 1) };
}

function rootConversation(): RouteConversation {
	return { kind: "root" };
}

function rootFocus(): ExecutionFocus {
	return { kind: "root" };
}

function executionRouteFor(route: WorkspaceRoute, view: ExecutionView): ExecutionRoute {
	if (route.destination === "execution") {
		return {
			...route,
			view,
			handoff: view === route.view ? route.handoff : null,
		};
	}
	return {
		destination: "execution",
		scope: route.scope,
		rootSessionId: route.rootSessionId,
		view,
		conversation: route.conversation,
		focus: rootFocus(),
		handoff: null,
	};
}

function agentConversationNavigation(
	route: WorkspaceRoute,
	sessionId: string,
	action: "agent-conversation" | "message-agent",
): RouteNavigation {
	const validatedSessionId = requireRouteId(sessionId, "agent session ID");
	const next: ConversationRoute = {
		destination: "conversation",
		scope: route.scope,
		rootSessionId: route.rootSessionId,
		conversation:
			validatedSessionId === route.rootSessionId
				? rootConversation()
				: { kind: "agent", sessionId: validatedSessionId },
	};
	return navigation("push", next, action);
}

function navigation(
	history: "push" | "replace",
	route: WorkspaceRoute,
	action: RouteNavigation["action"],
): RouteNavigation {
	return {
		kind: "route",
		history,
		route,
		url: serializeWorkspaceRoute(route),
		action,
	};
}

function invalidConversationWarning(
	requestedValue: string | null,
	reason: "malformed" | "wrong-root" | "unavailable" | "wrong-root-membership",
): WorkspaceRouteWarning {
	return {
		kind: "invalid-execution-conversation",
		persistent: true,
		reason,
		requestedValue,
		message: "The requested conversation was unavailable. The root conversation is shown instead.",
	};
}

function unsupportedQueryWarnings(
	query: ParsedQuery,
	hash: string,
	supportedParameters: ReadonlySet<string>,
): WorkspaceRouteWarning[] {
	const warnings: WorkspaceRouteWarning[] = [];
	const parameters = Array.from(query.values.keys()).filter((name) => !supportedParameters.has(name));
	if (parameters.length > 0) {
		warnings.push({
			kind: "unsupported-query",
			persistent: false,
			parameters: parameters.sort(),
			message: "Unsupported workspace query parameters were removed.",
		});
	}
	if (hash) {
		warnings.push({
			kind: "unsupported-fragment",
			persistent: false,
			message: "URL fragments are not supported for workspace routes and were removed.",
		});
	}
	return warnings;
}

function parseQuery(search: string): ParsedQuery | null {
	const values = new Map<string, string[]>();
	const raw = search.startsWith("?") ? search.slice(1) : search;
	if (!raw) return { values };
	for (const pair of raw.split("&")) {
		const separator = pair.indexOf("=");
		const rawName = separator === -1 ? pair : pair.slice(0, separator);
		const rawValue = separator === -1 ? "" : pair.slice(separator + 1);
		const name = decodeQueryPart(rawName);
		const value = decodeQueryPart(rawValue);
		if (name === null || value === null) return null;
		values.set(name, [...(values.get(name) ?? []), value]);
	}
	return { values };
}

function decodePathSegments(segments: string[]): string[] | null {
	const decoded: string[] = [];
	for (const segment of segments) {
		try {
			decoded.push(decodeURIComponent(segment));
		} catch {
			return null;
		}
	}
	return decoded;
}

function decodeQueryPart(value: string): string | null {
	try {
		return decodeURIComponent(value.replace(/\+/gu, " "));
	} catch {
		return null;
	}
}

function routeId(value: unknown): RouteId | null {
	if (
		typeof value !== "string" ||
		value.trim() === "" ||
		value === "." ||
		value === ".." ||
		INVALID_ID_CHARACTERS.test(value)
	) {
		return null;
	}
	try {
		encodeURIComponent(value);
		return value as RouteId;
	} catch {
		return null;
	}
}

function requireRouteId(value: unknown, label: string): RouteId {
	const validated = routeId(value);
	if (!validated) {
		throw new TypeError(
			`Workspace route programmer error: ${label} must be non-empty and cannot be ".", "..", or contain slashes, control characters, or malformed Unicode.`,
		);
	}
	return validated;
}

function assertWorkspaceRoute(route: WorkspaceRoute): void {
	if (!isRecord(route)) programmerError("route must be an object");
	const destination = route.destination;
	if (destination !== "conversation" && destination !== "execution") {
		programmerError("route destination must be conversation or execution");
	}

	assertExactKeys(
		route,
		destination === "conversation"
			? ["destination", "scope", "rootSessionId", "conversation"]
			: ["destination", "scope", "rootSessionId", "view", "conversation", "focus", "handoff"],
		"route",
	);
	assertRouteScope(route.scope);
	const rootSessionId = requireRouteId(route.rootSessionId, "root session ID");
	assertConversation(route.conversation, rootSessionId);
	if (destination === "conversation") return;

	if (!EXECUTION_VIEWS.has(route.view)) programmerError(`unsupported execution view "${String(route.view)}"`);
	assertFocus(route.focus, rootSessionId);
	if (route.handoff !== null) {
		if (!isRecord(route.handoff)) programmerError("handoff must be an object or null");
		assertExactKeys(route.handoff, ["kind", "ref"], "handoff");
		if (route.handoff.kind !== "handoff") programmerError("handoff kind must be handoff");
		requireRouteId(route.handoff.ref, "handoff reference");
	}
}

function assertRouteScope(scope: WorkspaceRouteScope): void {
	if (!isRecord(scope)) programmerError("scope must be an object");
	if (scope.kind === "host") {
		assertExactKeys(scope, ["kind"], "Host scope");
		return;
	}
	if (scope.kind === "project") {
		assertExactKeys(scope, ["kind", "projectId"], "project scope");
		requireRouteId(scope.projectId, "project ID");
		return;
	}
	programmerError("scope kind must be project or host");
}

function assertConversation(conversation: RouteConversation, rootSessionId: RouteId): void {
	if (!isRecord(conversation)) programmerError("conversation must be an object");
	if (conversation.kind === "root") {
		assertExactKeys(conversation, ["kind"], "root conversation");
		return;
	}
	if (conversation.kind === "agent") {
		assertExactKeys(conversation, ["kind", "sessionId"], "agent conversation");
		const sessionId = requireRouteId(conversation.sessionId, "conversation session ID");
		if (sessionId === rootSessionId) {
			programmerError("an agent conversation cannot repeat the root session ID; use kind root");
		}
		return;
	}
	programmerError("conversation kind must be root or agent");
}

function assertFocus(focus: ExecutionFocus, rootSessionId: RouteId): void {
	if (!isRecord(focus)) programmerError("focus must be an object");
	if (focus.kind === "root") {
		assertExactKeys(focus, ["kind"], "root focus");
		return;
	}
	if (focus.kind === "agent") {
		assertExactKeys(focus, ["kind", "sessionId"], "agent focus");
		const sessionId = requireRouteId(focus.sessionId, "focus agent session ID");
		if (sessionId === rootSessionId) {
			programmerError("an agent focus cannot repeat the root session ID; use kind root");
		}
		return;
	}
	if (focus.kind === "delegation") {
		assertExactKeys(focus, ["kind", "delegationId"], "delegation focus");
		requireRouteId(focus.delegationId, "delegation ID");
		return;
	}
	programmerError("focus kind must be root, agent, or delegation");
}

function assertExactKeys(record: Record<string, unknown>, allowed: readonly string[], label: string): void {
	const unexpected = Object.keys(record).filter((key) => !allowed.includes(key));
	if (unexpected.length > 0) {
		programmerError(`${label} contains unsupported state: ${unexpected.sort().join(", ")}`);
	}
}

function programmerError(message: string): never {
	throw new TypeError(`Workspace route programmer error: ${message}.`);
}

function encodePart(value: string): string {
	return encodeURIComponent(value).replace(/[!'()*]/gu, (character) =>
		`%${character.charCodeAt(0).toString(16).toUpperCase()}`,
	);
}

function ownsWorkspacePath(pathname: string): boolean {
	return pathname === "/w" || pathname.startsWith("/w/");
}

function locationParts(input: string | WorkspaceRouteLocation): ParsedLocation {
	if (typeof input !== "string") {
		const search = input.search ?? "";
		const hash = input.hash ?? "";
		return {
			pathname: input.pathname,
			search,
			hash,
			requestedUrl: `${input.pathname}${search}${hash}`,
		};
	}
	const hashOffset = input.indexOf("#");
	const withoutHash = hashOffset === -1 ? input : input.slice(0, hashOffset);
	const hash = hashOffset === -1 ? "" : input.slice(hashOffset);
	const searchOffset = withoutHash.indexOf("?");
	const pathname = searchOffset === -1 ? withoutHash : withoutHash.slice(0, searchOffset);
	const search = searchOffset === -1 ? "" : withoutHash.slice(searchOffset);
	return { pathname, search, hash, requestedUrl: input };
}

function recoveryForRootConversation(base: ParsedPathBase): WorkspaceRouteRecovery {
	return {
		label: "root-conversation",
		url: serializeWorkspaceRoute(rootConversationRoute(base.scope, base.rootSessionId)),
	};
}

function recoveryFromPartiallyDecodablePath(rawSegments: string[]): WorkspaceRouteRecovery | null {
	const decodedBase = decodePathBase(rawSegments);
	if (!decodedBase) return null;
	if (rawSegments[decodedBase.suffixOffset] === "conversation") {
		return recoveryForRootConversation(decodedBase);
	}
	if (rawSegments[decodedBase.suffixOffset] === "execution") {
		return rootOutlineRecovery(decodedBase);
	}
	return null;
}

function decodePathBase(raw: string[]): ParsedPathBase | null {
	try {
		if (raw[0] !== "" || raw[1] !== "w") return null;
		if (raw[2] === "project" && raw[4] === "run") {
			const projectId = routeId(decodeURIComponent(raw[3] ?? ""));
			const rootSessionId = routeId(decodeURIComponent(raw[5] ?? ""));
			if (!projectId || !rootSessionId) return null;
			return {
				scope: { kind: "project", projectId },
				rootSessionId,
				suffixOffset: 6,
			};
		}
		if (raw[2] === "host" && raw[3] === "run") {
			const rootSessionId = routeId(decodeURIComponent(raw[4] ?? ""));
			if (!rootSessionId) return null;
			return {
				scope: { kind: "host" },
				rootSessionId,
				suffixOffset: 5,
			};
		}
		return null;
	} catch {
		return null;
	}
}

function rootOutlineRecovery(base: ParsedPathBase): WorkspaceRouteRecovery {
	return {
		label: "root-outline",
		url: serializeWorkspaceRoute({
			destination: "execution",
			scope: base.scope,
			rootSessionId: base.rootSessionId,
			view: "overview",
			conversation: rootConversation(),
			focus: rootFocus(),
			handoff: null,
		}),
	};
}

function unavailable(
	issue: WorkspaceRouteUnavailableIssue,
	message: string,
	location: ParsedLocation,
	backTo: WorkspaceRouteRecovery | null,
): WorkspaceRouteUnavailable {
	return {
		kind: "unavailable",
		issue,
		message,
		requestedUrl: location.requestedUrl,
		backTo,
	};
}

function workspaceHistoryMarker(state: unknown): WorkspaceHistoryMarker["piRelayWorkspaceRoute"] | null {
	if (!isRecord(state) || !isRecord(state.piRelayWorkspaceRoute)) return null;
	const marker = state.piRelayWorkspaceRoute;
	if (marker.version !== 1 || typeof marker.action !== "string") return null;
	return marker as unknown as WorkspaceHistoryMarker["piRelayWorkspaceRoute"];
}

function isRecord(value: unknown): value is Record<string, unknown> {
	return typeof value === "object" && value !== null && !Array.isArray(value);
}

function unique<T>(values: T[]): T[] {
	return Array.from(new Set(values));
}
