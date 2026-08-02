import { FolderTree, Loader2, Plug, RotateCcw, TriangleAlert } from "lucide-react";
import { useState } from "react";
import { Alert, AlertAction, AlertDescription, AlertTitle } from "@/components/ui/alert";
import { Button } from "@/components/ui/button";
import { Skeleton } from "@/components/ui/skeleton";
import { Spinner } from "@/components/ui/spinner";
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
	workspacePreparationStatus,
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
	workspacePreparationStatus: string | null;
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
								{workspacePreparationStatus ? (
									<p
										className="new-session-setup-status workspace-preparation-status"
										role="status"
										aria-label={workspacePreparationStatus}
									>
										<Loader2 className="spin" size={14} aria-hidden />
										<span>{workspacePreparationStatus}</span>
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
											<McpSetupSkeleton label="Loading MCP tools" />
										) : null}
									</>
								)}
								{showMcp && mcpLoading ? (
									<div className="mcp-setup-progress" role="status" aria-label="Refreshing MCP tools">
										<Spinner aria-hidden />
										<span>Refreshing MCP tools…</span>
									</div>
								) : null}
								{!mcpError && !mcpLoading && !mcpConfigurationReady ? (
									mcpAuthMutationBlockedReason ? (
										showMcp ? null : (
											<Alert className="mcp-setup-alert" role="status">
												<Plug aria-hidden />
												<AlertDescription>{mcpAuthMutationBlockedReason}</AlertDescription>
											</Alert>
										)
									) : (
										<McpSetupSkeleton label="Loading MCP tools" compact />
									)
								) : null}
								{mcpError ? (
									<Alert className="mcp-setup-alert" variant="destructive">
										<TriangleAlert aria-hidden />
										<AlertTitle>MCP unavailable</AlertTitle>
										<AlertDescription>{mcpError}</AlertDescription>
										<AlertAction>
											<Button
												type="button"
												variant="ghost"
												size="sm"
												onClick={onRetryMcp}
												disabled={disabled || mcpLoading}
											>
												<RotateCcw data-icon="inline-start" aria-hidden />
											Retry
											</Button>
										</AlertAction>
									</Alert>
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

function McpSetupSkeleton({ label, compact = false }: { label: string; compact?: boolean }) {
	return (
		<div className="mcp-setup-skeleton" role="status">
			<span className="sr-only">{label}</span>
			<div className="mcp-setup-skeleton-row">
				<Spinner aria-hidden />
				<Skeleton className="h-3 w-28" />
			</div>
			{compact ? null : <Skeleton className="h-12 w-full" />}
		</div>
	);
}
