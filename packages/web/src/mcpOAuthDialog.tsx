import { useRef, useState, type RefObject } from "react";
import { Check, Clock3, Copy, ExternalLink, MonitorSmartphone, TriangleAlert } from "lucide-react";
import { Alert, AlertDescription } from "@/components/ui/alert";
import { Button } from "@/components/ui/button";
import {
	Field,
	FieldGroup,
	FieldLabel,
} from "@/components/ui/field";
import {
	InputGroup,
	InputGroupAddon,
	InputGroupButton,
	InputGroupInput,
} from "@/components/ui/input-group";
import { Spinner } from "@/components/ui/spinner";
import { Textarea } from "@/components/ui/textarea";
import {
	Tooltip,
	TooltipContent,
	TooltipProvider,
	TooltipTrigger,
} from "@/components/ui/tooltip";
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
	const expiration = new Date(login.expires_at_unix_seconds * 1000);

	return (
		<TooltipProvider>
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
						<DialogDescription>OAuth authorization</DialogDescription>
					</DialogHeading>
					<time
						className="mcp-oauth-expiration"
						dateTime={expiration.toISOString()}
						title={`Expires at ${expiration.toLocaleTimeString()}`}
					>
						<Clock3 aria-hidden />
						Expires {expiration.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" })}
					</time>
					<DialogCloseButton label="cancel MCP login" disabled={actionBlocked} />
				</DialogHeader>
				<DialogBody className="mcp-oauth-dialog-body">
					<Button asChild className="mcp-oauth-open">
						<a
							ref={openLinkRef}
							href={login.authorization_url}
							target="_blank"
							rel="noopener noreferrer"
						>
							<ExternalLink data-icon="inline-start" aria-hidden />
							Authorize
						</a>
					</Button>
					<FieldGroup>
						<Field>
							<FieldLabel htmlFor="mcp-authorization-url">Authorization URL</FieldLabel>
							<InputGroup>
								<InputGroupInput
									id="mcp-authorization-url"
									className="mcp-oauth-mono"
									value={login.authorization_url}
									readOnly
								/>
								<InputGroupAddon align="inline-end">
									<Tooltip>
										<TooltipTrigger asChild>
											<InputGroupButton
												size="icon-xs"
												aria-label={copied ? "Authorization URL copied" : "Copy authorization URL"}
												onClick={() => void copy()}
												disabled={busy}
											>
												{busyAction === "copy"
													? <Spinner />
													: copied
													? <Check aria-hidden />
													: <Copy aria-hidden />}
											</InputGroupButton>
										</TooltipTrigger>
										<TooltipContent>{copied ? "Copied" : "Copy URL"}</TooltipContent>
									</Tooltip>
								</InputGroupAddon>
							</InputGroup>
						</Field>
						<Field>
							<FieldLabel htmlFor="mcp-callback-url">
								Remote callback URL
								<MonitorSmartphone
									className="mcp-oauth-field-hint"
									aria-hidden
								/>
							</FieldLabel>
							<Textarea
								id="mcp-callback-url"
								className="mcp-oauth-mono"
								value={callbackUrl}
								onChange={(event) => setCallbackUrl(event.target.value)}
								maxLength={MAX_MCP_CALLBACK_URL_LENGTH}
								rows={3}
								placeholder="http://127.0.0.1:…/callback?code=…"
								disabled={actionBlocked}
							/>
						</Field>
					</FieldGroup>
					{error ? (
						<Alert variant="destructive">
							<TriangleAlert aria-hidden />
							<AlertDescription>{error}</AlertDescription>
						</Alert>
					) : null}
					{mutationBlockedReason ? (
						<Alert variant="destructive" role="status">
							<TriangleAlert aria-hidden />
							<AlertDescription>{mutationBlockedReason}</AlertDescription>
						</Alert>
					) : null}
				</DialogBody>
				<DialogFooter>
					<Button
						type="button"
						variant="outline"
						onClick={cancel}
						disabled={actionBlocked}
					>
						{busyAction === "cancel" ? <Spinner data-icon="inline-start" /> : null}
						Cancel
					</Button>
					<Button
						type="button"
						onClick={() => void run("complete", () => onComplete(callbackUrl.trim()))}
						disabled={actionBlocked || callbackUrl.trim().length === 0}
						aria-busy={busyAction === "complete"}
					>
						{busyAction === "complete" ? <Spinner data-icon="inline-start" /> : null}
						Complete
					</Button>
				</DialogFooter>
			</AppDialog>
		</TooltipProvider>
	);
}

function errorMessage(error: unknown): string {
	return error instanceof Error ? error.message : String(error);
}
