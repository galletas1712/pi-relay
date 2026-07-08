import { AlertTriangle, Loader2, RefreshCw, WifiOff } from "lucide-react";
import { useEffect, useRef, useState } from "react";
import { parseSlash } from "./slash.ts";
import type { ConnectionStatus } from "./rpc.ts";

export const WAITING_FOR_CONNECTION = "Waiting for connection";

export interface ConnectionDisplayState {
	kind: "connecting" | "unavailable";
	title: string;
	detail: string;
	canRetry: boolean;
}

export function connectionDisplayState(
	status: ConnectionStatus,
	hasConnected: boolean,
	retrying = false,
): ConnectionDisplayState | null {
	if (status === "open") return null;
	if (retrying || status === "connecting") {
		return {
			kind: "connecting",
			title: retrying || hasConnected ? "Reconnecting…" : "Connecting…",
			detail: "Drafting and Help stay available; already loaded reading, navigation, export, and history stay local.",
			canRetry: retrying,
		};
	}
	return {
		kind: "unavailable",
		title: status === "error" ? "Connection error" : "Connection closed",
		detail: `${WAITING_FOR_CONNECTION}. Drafting and cached content remain available.`,
		canRetry: true,
	};
}

export function remoteActionBlockedReason(status: ConnectionStatus): string | null {
	return status === "open" ? null : WAITING_FOR_CONNECTION;
}

export function firstDisabledReason(...reasons: (string | null | undefined | false)[]): string | null {
	return reasons.find((reason): reason is string => typeof reason === "string" && reason.length > 0) ?? null;
}

export function assertRemoteActionAllowed(reason: string | null): void {
	if (reason) throw new Error(reason);
}

export function assertMutationAllowed(reason: string | null): void {
	assertRemoteActionAllowed(reason);
}

export function composerTextNeedsConnection(
	text: string,
	options: { cachedHistoryAvailable?: boolean } = {},
): boolean {
	const slash = parseSlash(text);
	if (!slash) return true;
	if (slash.name === "switch") return !options.cachedHistoryAvailable;
	// /export operates only on entries already in memory while disconnected.
	return !["", "help", "export"].includes(slash.name);
}

export class ConnectionRetryController {
	private generation = 0;
	private pending: Promise<void> | null = null;

	retry(
		connect: () => Promise<void>,
		onFailure: (error: unknown) => void,
		onSettled: () => void,
	): Promise<void> {
		if (this.pending) return this.pending;
		const generation = ++this.generation;
		let attempt: Promise<void>;
		try {
			attempt = connect();
		} catch (error) {
			attempt = Promise.reject(error);
		}
		const pending = attempt.then(
			() => undefined,
			(error) => {
				if (generation === this.generation) onFailure(error);
			},
		).finally(() => {
			if (generation !== this.generation) return;
			this.pending = null;
			onSettled();
		});
		this.pending = pending;
		return pending;
	}

	opened(): void {
		this.generation += 1;
		this.pending = null;
	}

	isPending(): boolean {
		return this.pending !== null;
	}
}

export function ConnectionBlockedReason({
	reason,
	className = "",
}: {
	reason: string | null | undefined;
	className?: string;
}) {
	if (!reason) return null;
	return (
		<span className={`connection-blocked-reason ${className}`.trim()} tabIndex={0}>
			{reason}
		</span>
	);
}

export function ConnectionRecoveryBanner({
	status,
	hasConnected,
	retrying,
	onRetry,
}: {
	status: ConnectionStatus;
	hasConnected: boolean;
	retrying: boolean;
	onRetry: () => void;
}) {
	const display = connectionDisplayState(status, hasConnected, retrying);
	const announcedFailureRef = useRef(false);
	const [failureAnnouncement, setFailureAnnouncement] = useState("");

	useEffect(() => {
		if (!display) {
			announcedFailureRef.current = false;
			setFailureAnnouncement("");
			return;
		}
		if (display.kind !== "unavailable" || announcedFailureRef.current) return;
		announcedFailureRef.current = true;
		setFailureAnnouncement(`${display.title}. ${WAITING_FOR_CONNECTION}.`);
	}, [display]);

	if (!display) return null;
	const Icon = display.kind === "unavailable" ? (status === "error" ? AlertTriangle : WifiOff) : Loader2;
	return (
		<div
			className={`connection-recovery-banner ${display.kind}`}
			role="status"
			aria-live="off"
			aria-label={`${display.title} ${display.detail}`}
		>
			<Icon className={display.kind === "connecting" ? "spin" : undefined} size={15} aria-hidden />
			<div className="connection-recovery-copy">
				<strong>{display.title}</strong>
				<span>{display.detail}</span>
			</div>
			{display.canRetry ? (
				<button
					type="button"
					className="secondary-button connection-retry-button"
					disabled={retrying}
					aria-busy={retrying}
					onClick={onRetry}
				>
					{retrying ? <Loader2 className="spin" size={13} aria-hidden /> : <RefreshCw size={13} aria-hidden />}
					{retrying ? "Retrying…" : "Retry connection"}
				</button>
			) : null}
			{failureAnnouncement ? <span className="sr-only" role="alert">{failureAnnouncement}</span> : null}
		</div>
	);
}
