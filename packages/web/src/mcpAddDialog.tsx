import type { RefObject } from "react";
import { ConnectionBlockedReason } from "./connectionRecovery.tsx";
import {
	AppDialog,
	DialogBody,
	DialogClose,
	DialogCloseButton,
	DialogDescription,
	DialogFooter,
	DialogHeader,
	DialogHeading,
	DialogTitle,
} from "./dialog.tsx";
import type { McpSelectionState } from "./mcpSelection.ts";
import { mcpSelectionPayload } from "./mcpSelection.ts";
import { McpToolPicker } from "./mcpToolPicker.tsx";
import type { McpInventory } from "./types.ts";

export function McpAddDialog({
	inventory,
	selection,
	lockedSelection,
	loading,
	error,
	onChange,
	onRetry,
	onClose,
	onSubmit,
	mutationBlockedReason,
	returnFocusFallbackRef,
}: {
	inventory: McpInventory | null;
	selection: McpSelectionState;
	lockedSelection: McpSelectionState;
	loading: boolean;
	error: string | null;
	onChange: (selection: McpSelectionState) => void;
	onRetry: () => void;
	onClose: () => void;
	onSubmit: () => void | Promise<void>;
	mutationBlockedReason?: string | null;
	returnFocusFallbackRef?: RefObject<HTMLElement | null>;
}) {
	const additions = inventory
		? mcpSelectionPayload(inventory, selection, lockedSelection)
		: undefined;
	return (
		<AppDialog
			className="rename-dialog mcp-add-dialog"
			busy={loading}
			returnFocusFallbackRef={returnFocusFallbackRef}
			onDismiss={onClose}
		>
			<DialogHeader>
				<DialogHeading>
					<DialogTitle>Add MCP tools</DialogTitle>
					<DialogDescription>
						Existing tools stay selected. Adding tools rerenders this session’s system prompt.
					</DialogDescription>
				</DialogHeading>
				<DialogCloseButton label="close add MCP tools dialog" disabled={loading} />
			</DialogHeader>
			<DialogBody className="mcp-add-dialog-body">
				{inventory ? (
					<McpToolPicker
						inventory={inventory}
						selection={selection}
						lockedSelection={lockedSelection}
						onChange={onChange}
						disabled={loading}
						inventoryReady={!loading && !error}
						authStatusRequired={false}
					/>
				) : loading ? (
					<p role="status">Loading MCP tools…</p>
				) : null}
				{error ? (
					<div className="new-session-setup-error" role="alert">
						<span>MCP tools unavailable: {error}</span>
						<button type="button" onClick={onRetry} disabled={loading}>Retry</button>
					</div>
				) : null}
			</DialogBody>
			<DialogFooter>
				<ConnectionBlockedReason reason={mutationBlockedReason} className="dialog-blocked-reason" />
				<DialogClose className="secondary-button" disabled={loading}>Cancel</DialogClose>
				<button
					type="button"
					className="primary-button"
					disabled={loading || !!error || !additions || !!mutationBlockedReason}
					onClick={onSubmit}
				>
					{loading ? "Loading…" : "Add tools"}
				</button>
			</DialogFooter>
		</AppDialog>
	);
}
