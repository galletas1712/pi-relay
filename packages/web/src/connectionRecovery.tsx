import { Loader2, RefreshCw, WifiOff } from "lucide-react";
import { parseSlash } from "./slash.ts";
import type { ConnectionStatus } from "./rpc.ts";

export const WAITING_FOR_CONNECTION = "Waiting for connection";

export function remoteActionBlockedReason(status: ConnectionStatus): string | null {
	return status === "open" ? null : WAITING_FOR_CONNECTION;
}

export function firstDisabledReason(...reasons: (string | null | undefined | false)[]): string | null {
	return reasons.find((reason): reason is string => typeof reason === "string" && reason.length > 0) ?? null;
}

export function assertRemoteActionAllowed(reason: string | null): void {
	if (reason) throw new Error(reason);
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
	disconnected,
	retrying,
	onRetry,
}: {
	disconnected: boolean;
	retrying: boolean;
	onRetry: () => void;
}) {
	if (!disconnected) return null;
	return (
		<div
			className="connection-recovery-banner"
			role="status"
			aria-label="Disconnected"
		>
			<WifiOff size={15} aria-hidden />
			<strong className="connection-recovery-label">Disconnected</strong>
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
		</div>
	);
}
