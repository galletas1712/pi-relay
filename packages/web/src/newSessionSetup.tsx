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
					<h1>New session</h1>
				</header>
				{showWorkspaceSection || showMcpSection ? (
					<div className="new-session-setup-manifest">
						{showWorkspaceSection ? (
							<section className="new-session-setup-section" aria-label="Workspaces">
								{showWorkspaces ? (
									<WorkspaceScopePicker
										scope={workspaceScope}
										onChange={onWorkspaceScopeChange}
										disabled={disabled}
										open={open === "workspaces"}
										onOpenChange={(nextOpen) => setOpen(nextOpen ? "workspaces" : null)}
									/>
								) : (
									<>
										<div className="new-session-setup-static-header">
											<FolderTree size={18} aria-hidden />
											<h2>Workspaces</h2>
										</div>
										{workspaceConfiguration.status === "loading" ? (
											<p className="new-session-setup-status" role="status">
												Loading workspaces…
											</p>
										) : (
											<p className="new-session-setup-error">
												<span>Workspaces unavailable</span>
												<span>Retry in Projects</span>
											</p>
										)}
									</>
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
							<section className="new-session-setup-section" aria-label="MCP tools">
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
								) : (
									<>
										<div className="new-session-setup-static-header">
											<Plug size={18} aria-hidden />
											<h2>MCP tools</h2>
										</div>
										{mcpLoading ? (
											<p className="new-session-setup-status" role="status">
												Loading MCP tools…
											</p>
										) : null}
									</>
								)}
								{showMcp && mcpLoading ? (
									<p className="new-session-setup-status" role="status">
										Refreshing MCP tools…
									</p>
								) : null}
								{!mcpError && !mcpLoading && !mcpConfigurationReady ? (
									<p className="new-session-setup-status" role="status">
										{mcpAuthMutationBlockedReason ?? "Loading MCP tools…"}
									</p>
								) : null}
								{mcpError ? (
									<div className="new-session-setup-error" role="alert">
										<span>MCP tools unavailable</span>
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
						<h2>Host context only</h2>
					</div>
				)}
			</div>
		</div>
	);
}
