import { useState } from "react";
import { McpToolPicker } from "./mcpToolPicker.tsx";
import type { McpSelectionState } from "./mcpSelection.ts";
import type { McpAuthServerStatus, McpInventory } from "./types.ts";
import type { WorkspaceScopeEntry } from "./workspaceScope.ts";
import { WorkspaceScopePicker } from "./workspaceScopePicker.tsx";

type OpenSetup = "workspaces" | "mcp" | null;

export function NewSessionSetup({
	workspaceScope,
	onWorkspaceScopeChange,
	mcpInventory,
	mcpSelection,
	onMcpSelectionChange,
	mcpLoading,
	mcpReady,
	mcpError,
	onRetryMcp,
	mcpAuthStatus,
	mcpAuthStatusReady,
	onMcpLogin,
	onMcpLogout,
	mcpAuthBusyServer,
	mcpAuthMutationBlockedReason,
	disabled,
}: {
	workspaceScope: WorkspaceScopeEntry[] | null;
	onWorkspaceScopeChange: (scope: WorkspaceScopeEntry[]) => void;
	mcpInventory: McpInventory | null;
	mcpSelection: McpSelectionState;
	onMcpSelectionChange: (selection: McpSelectionState) => void;
	mcpLoading: boolean;
	mcpReady: boolean;
	mcpError: string | null;
	onRetryMcp: () => void;
	mcpAuthStatus: McpAuthServerStatus[];
	mcpAuthStatusReady: boolean;
	onMcpLogin: (server: string) => void;
	onMcpLogout: (server: string) => void;
	mcpAuthBusyServer?: string | null;
	mcpAuthMutationBlockedReason?: string | null;
	disabled?: boolean;
}) {
	const [open, setOpen] = useState<OpenSetup>(null);
	const showWorkspaces = !!workspaceScope?.length;
	const showMcp = !!mcpInventory?.servers.length || mcpAuthStatus.length > 0;
	if (!showWorkspaces && !showMcp && !mcpLoading && !mcpError) return null;

	return (
		<div className="new-session-setup">
			{showWorkspaces ? (
				<WorkspaceScopePicker
					scope={workspaceScope}
					onChange={onWorkspaceScopeChange}
					disabled={disabled}
					open={open === "workspaces"}
					onOpenChange={(nextOpen) => setOpen(nextOpen ? "workspaces" : null)}
				/>
			) : null}
			{showMcp ? (
				<McpToolPicker
					inventory={mcpInventory ?? { revision: "", servers: [] }}
					selection={mcpSelection}
					onChange={onMcpSelectionChange}
					disabled={disabled}
					inventoryReady={mcpReady}
					open={open === "mcp"}
					onOpenChange={(nextOpen) => setOpen(nextOpen ? "mcp" : null)}
					authStatus={mcpAuthStatus}
					authStatusReady={mcpAuthStatusReady}
					onLogin={onMcpLogin}
					onLogout={onMcpLogout}
					authBusyServer={mcpAuthBusyServer}
					authMutationBlockedReason={mcpAuthMutationBlockedReason}
				/>
			) : mcpLoading ? (
				<p className="new-session-setup-status" role="status">
					MCP tools · Loading…
				</p>
			) : null}
			{showMcp && mcpLoading ? (
				<p className="new-session-setup-status" role="status">
					MCP tools · Refreshing…
				</p>
			) : null}
			{mcpError ? (
				<div className="new-session-setup-error" role="alert">
					<span>MCP tools unavailable. You can start without them.</span>
					<button type="button" onClick={onRetryMcp} disabled={disabled || mcpLoading}>
						Retry
					</button>
				</div>
			) : null}
		</div>
	);
}
