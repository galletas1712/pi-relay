import { useRef, useState, type RefObject } from "react";
import {
	AppDialog,
	DialogBody,
	DialogCloseButton,
	DialogDescription,
	DialogFooter,
	DialogHeader,
	DialogHeading,
	DialogTitle,
} from "./dialog.tsx";
import type { McpLoginResult } from "./types.ts";

export const MAX_MCP_CALLBACK_URL_LENGTH = 16 * 1024;

export function McpOAuthDialog({
	server,
	login,
	onComplete,
	onCancel,
	mutationBlockedReason,
	returnFocusFallbackRef,
}: {
	server: string;
	login: McpLoginResult;
	onComplete: (callbackUrl: string) => Promise<void>;
	onCancel: () => Promise<void>;
	mutationBlockedReason?: string | null;
	returnFocusFallbackRef?: RefObject<HTMLElement | null>;
}) {
	const openLinkRef = useRef<HTMLAnchorElement>(null);
	const [callbackUrl, setCallbackUrl] = useState("");
	const [busyAction, setBusyAction] = useState<"complete" | "cancel" | "copy" | null>(null);
	const [copied, setCopied] = useState(false);
	const [error, setError] = useState<string | null>(null);
	const busy = busyAction !== null;
	const actionBlocked = busy || !!mutationBlockedReason;

	const run = async (action: "complete" | "cancel", operation: () => Promise<void>) => {
		if (busy) return;
		setBusyAction(action);
		setError(null);
		try {
			await operation();
		} catch (operationError) {
			setError(errorMessage(operationError));
		} finally {
			setBusyAction(null);
		}
	};
	const cancel = () => {
		void run("cancel", onCancel);
	};
	const copy = async () => {
		if (busy) return;
		setBusyAction("copy");
		setCopied(false);
		setError(null);
		try {
			await navigator.clipboard.writeText(login.authorization_url);
			setCopied(true);
		} catch (copyError) {
			setError(errorMessage(copyError));
		} finally {
			setBusyAction(null);
		}
	};

	return (
		<AppDialog
			className="rename-dialog mcp-oauth-dialog"
			busy={actionBlocked}
			initialFocusRef={openLinkRef}
			returnFocusFallbackRef={returnFocusFallbackRef}
			onDismiss={cancel}
		>
			<DialogHeader>
				<DialogHeading>
					<DialogTitle>Log in to {server}</DialogTitle>
					<DialogDescription>
						The daemon is listening for this OAuth callback on its own loopback interface.
					</DialogDescription>
				</DialogHeading>
				<DialogCloseButton label="cancel MCP login" disabled={actionBlocked} />
			</DialogHeader>
			<DialogBody className="mcp-oauth-dialog-body">
				<a
					ref={openLinkRef}
					className="primary-button mcp-oauth-open"
					href={login.authorization_url}
					target="_blank"
					rel="noopener noreferrer"
				>
					Open authorization page
				</a>
				<div className="mcp-oauth-url">
					<label htmlFor="mcp-authorization-url">Authorization URL</label>
					<div>
						<input id="mcp-authorization-url" value={login.authorization_url} readOnly />
						<button type="button" className="secondary-button" onClick={() => void copy()} disabled={busy}>
							{busyAction === "copy" ? "Copying…" : copied ? "Copied" : "Copy"}
						</button>
					</div>
				</div>
				<label className="rename-field" htmlFor="mcp-callback-url">
					<span>Callback URL for a remote daemon</span>
					<textarea
						id="mcp-callback-url"
						value={callbackUrl}
						onChange={(event) => setCallbackUrl(event.target.value)}
						maxLength={MAX_MCP_CALLBACK_URL_LENGTH}
						rows={3}
						placeholder="Paste the entire http://127.0.0.1:… callback URL"
						disabled={actionBlocked}
					/>
				</label>
				<p className="muted">
					If this browser and the daemon are on different machines, copy the entire URL from the browser after authorization and paste it here. Do not paste only the code.
				</p>
				<p className="muted">
					This login expires at{" "}
					<time dateTime={new Date(login.expires_at_unix_seconds * 1000).toISOString()}>
						{new Date(login.expires_at_unix_seconds * 1000).toLocaleTimeString()}
					</time>.
				</p>
				{error ? <p className="error-text" role="alert">{error}</p> : null}
				{mutationBlockedReason ? (
					<p className="error-text" role="status">{mutationBlockedReason}</p>
				) : null}
			</DialogBody>
			<DialogFooter>
				<button
					type="button"
					className="secondary-button"
					onClick={cancel}
					disabled={actionBlocked}
				>
					{busyAction === "cancel" ? "Cancelling…" : "Cancel"}
				</button>
				<button
					type="button"
					className="primary-button"
					onClick={() => void run("complete", () => onComplete(callbackUrl.trim()))}
					disabled={
						actionBlocked ||
						callbackUrl.trim().length === 0
					}
					aria-busy={busyAction === "complete"}
				>
					{busyAction === "complete" ? "Completing…" : "Complete"}
				</button>
			</DialogFooter>
		</AppDialog>
	);
}

function errorMessage(error: unknown): string {
	return error instanceof Error ? error.message : String(error);
}
