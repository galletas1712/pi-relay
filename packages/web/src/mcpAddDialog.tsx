import type { RefObject } from "react";
import { RotateCcw, TriangleAlert } from "lucide-react";
import { Alert, AlertAction, AlertDescription } from "@/components/ui/alert";
import { Button, buttonVariants } from "@/components/ui/button";
import { Skeleton } from "@/components/ui/skeleton";
import { Spinner } from "@/components/ui/spinner";
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
					<DialogDescription>Extend this session’s capabilities.</DialogDescription>
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
						defaultOpen
					/>
				) : loading ? (
					<McpPickerSkeleton label="Loading MCP tools" />
				) : null}
				{error ? (
					<Alert variant="destructive">
						<TriangleAlert aria-hidden />
						<AlertDescription>{error}</AlertDescription>
						<AlertAction>
							<Button
								type="button"
								variant="ghost"
								size="sm"
								onClick={onRetry}
								disabled={loading}
							>
								<RotateCcw data-icon="inline-start" aria-hidden />
								Retry
							</Button>
						</AlertAction>
					</Alert>
				) : null}
			</DialogBody>
			<DialogFooter>
				<ConnectionBlockedReason reason={mutationBlockedReason} className="dialog-blocked-reason" />
				<DialogClose className={buttonVariants({ variant: "outline" })} disabled={loading}>
					Cancel
				</DialogClose>
				<Button
					type="button"
					disabled={loading || !!error || !additions || !!mutationBlockedReason}
					onClick={onSubmit}
				>
					{loading ? <Spinner data-icon="inline-start" /> : null}
					Add tools
				</Button>
			</DialogFooter>
		</AppDialog>
	);
}

function McpPickerSkeleton({ label }: { label: string }) {
	return (
		<div className="mcp-picker-skeleton" role="status" aria-label={label}>
			<div className="mcp-picker-skeleton-heading">
				<Skeleton className="size-8" />
				<Skeleton className="h-4 w-32" />
			</div>
			<Skeleton className="h-16 w-full" />
			<Skeleton className="h-16 w-full" />
		</div>
	);
}
