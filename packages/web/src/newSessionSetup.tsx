import { FolderTree, Loader2, Plug } from "lucide-react";
import { useState } from "react";
import { McpToolPicker } from "./mcpToolPicker.tsx";
import type { McpSelectionState } from "./mcpSelection.ts";
import type { McpAuthServerStatus, McpInventory } from "./types.ts";
import type { WorkspaceScopeEntry } from "./workspaceScope.ts";
import { WorkspaceScopePicker } from "./workspaceScopePicker.tsx";

type OpenSetup = "workspaces" | "mcp" | null;

export type WorkspaceConfiguration =
	| { status: "loading" }
	| { status: "ready"; scope: WorkspaceScopeEntry[] | null }
	| { status: "unavailable" };

export function NewSessionSetup({
	workspaceConfiguration,
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
	preparingWorkspaces,
}: {
	workspaceConfiguration: WorkspaceConfiguration;
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
	preparingWorkspaces: boolean;
}) {
	const [open, setOpen] = useState<OpenSetup>(null);
	const workspaceScope =
		workspaceConfiguration.status === "ready" ? workspaceConfiguration.scope : null;
	const showWorkspaces = !!workspaceScope?.length;
	const showWorkspaceSection = showWorkspaces || workspaceConfiguration.status !== "ready";
	const showMcp = !!mcpInventory?.servers.length || mcpAuthStatus.length > 0;
	const mcpConfigurationReady = mcpReady && mcpAuthStatusReady;
	const showMcpSection = showMcp || mcpLoading || !!mcpError || !mcpConfigurationReady;

	return (
		<div className="new-session-setup" data-slot="new-session-setup">
			<div className="new-session-setup-inner">
				<header className="new-session-setup-header">
					<p>New session</p>
					<h1>Choose the context to bring in</h1>
					<span>
						Keep the session focused: include only the workspaces and remote tools it needs.
					</span>
				</header>
				{showWorkspaceSection || showMcpSection ? (
					<div className="new-session-setup-grid">
						{showWorkspaceSection ? (
							<section className="new-session-setup-card" aria-labelledby="new-session-workspaces-title">
								<div className="new-session-setup-card-header">
									<FolderTree size={18} aria-hidden />
									<div>
										<h2 id="new-session-workspaces-title">Workspace scope</h2>
										<p>Selected folders become the local file context for this session.</p>
									</div>
								</div>
								{showWorkspaces ? (
									<WorkspaceScopePicker
										scope={workspaceScope}
										onChange={onWorkspaceScopeChange}
										disabled={disabled}
										open={open === "workspaces"}
										onOpenChange={(nextOpen) => setOpen(nextOpen ? "workspaces" : null)}
									/>
								) : workspaceConfiguration.status === "loading" ? (
									<p className="new-session-setup-status" role="status">
										Loading project workspaces…
									</p>
								) : (
									<p className="new-session-setup-error">
										Workspace configuration unavailable. Retry from the Projects panel.
									</p>
								)}
								{preparingWorkspaces ? (
									<p
										className="new-session-setup-status workspace-preparation-status"
										role="status"
										aria-label="Preparing workspaces…"
									>
										<Loader2 className="spin" size={14} aria-hidden />
										<span>Preparing workspaces…</span>
									</p>
								) : null}
							</section>
						) : null}
						{showMcpSection ? (
							<section className="new-session-setup-card" aria-labelledby="new-session-mcp-title">
								<div className="new-session-setup-card-header">
									<Plug size={18} aria-hidden />
									<div>
										<h2 id="new-session-mcp-title">MCP servers &amp; tools</h2>
										<p>Remote tools add capabilities and context tokens to every agent.</p>
									</div>
								</div>
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
										Loading daemon-configured MCP servers…
									</p>
								) : null}
								{showMcp && mcpLoading ? (
									<p className="new-session-setup-status" role="status">
										MCP tools · Refreshing…
									</p>
								) : null}
								{!mcpError && !mcpLoading && !mcpConfigurationReady ? (
									<p className="new-session-setup-status" role="status">
										{mcpAuthMutationBlockedReason ?? "Waiting for MCP configuration…"}
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
							</section>
						) : null}
					</div>
				) : (
					<div className="new-session-setup-empty">
						<h2>No optional context configured</h2>
						<p>Write your first message below to start with the host environment.</p>
					</div>
				)}
			</div>
		</div>
	);
}
