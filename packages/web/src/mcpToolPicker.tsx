import { memo, useId, useState } from "react";
import {
	ChevronDown,
	ChevronRight,
	LockKeyhole,
	LogIn,
	LogOut,
	Plug,
	RotateCcw,
	ServerCog,
	TriangleAlert,
	Unplug,
	UsersRound,
	X,
} from "lucide-react";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Checkbox } from "@/components/ui/checkbox";
import { Spinner } from "@/components/ui/spinner";
import {
	mcpSelectionTotals,
	clearMcpServerSelection,
	inventoryWithSelection,
	serverSelectionState,
	toggleServer,
	toggleTool,
	type McpSelectionState,
} from "./mcpSelection.ts";
import type { McpAuthServerStatus, McpInventory, McpInventoryServer } from "./types.ts";

export const McpToolPicker = memo(function McpToolPicker({
	inventory,
	selection,
	lockedSelection = new Map(),
	onChange,
	disabled,
	inventoryReady = true,
	open: controlledOpen,
	defaultOpen = false,
	onOpenChange,
	authStatus = [],
	authStatusRequired = true,
	authStatusReady = true,
	onLogin,
	onLogout,
	authBusyServer = null,
	authMutationBlockedReason = null,
}: {
	inventory: McpInventory;
	selection: McpSelectionState;
	lockedSelection?: McpSelectionState;
	onChange: (selection: McpSelectionState) => void;
	disabled?: boolean;
	inventoryReady?: boolean;
	open?: boolean;
	defaultOpen?: boolean;
	onOpenChange?: (open: boolean) => void;
	authStatus?: McpAuthServerStatus[];
	authStatusRequired?: boolean;
	authStatusReady?: boolean;
	onLogin?: (server: string) => void;
	onLogout?: (server: string) => void;
	authBusyServer?: string | null;
	authMutationBlockedReason?: string | null;
}) {
	const idPrefix = useId();
	const [internalOpen, setInternalOpen] = useState(defaultOpen);
	const open = controlledOpen ?? internalOpen;
	const [expanded, setExpanded] = useState<ReadonlySet<string>>(new Set());
	const pickerInventory = inventoryWithSelection(inventory, lockedSelection);
	const inventoryByServer = new Map(pickerInventory.servers.map((server) => [server.server, server]));
	const authByServer = new Map(authStatus.map((status) => [status.server, status]));
	const serverIds = [...new Set([
		...authStatus.map((status) => status.server),
		...pickerInventory.servers.map((server) => server.server),
	])].sort();
	if (!serverIds.length) return null;
	const panelId = `${idPrefix}-mcp-panel`;
	const safetyDescriptionId = `${idPrefix}-mcp-safety`;
	const total = mcpSelectionTotals(pickerInventory, selection);
	const selectionStatus = mcpSelectionStatus(total.tools, total.contextTokens);
	const setOpen = (nextOpen: boolean) => {
		if (controlledOpen === undefined) setInternalOpen(nextOpen);
		onOpenChange?.(nextOpen);
	};
	const toggleExpanded = (server: string) => {
		const next = new Set(expanded);
		if (next.has(server)) next.delete(server);
		else next.add(server);
		setExpanded(next);
	};

	return (
			<div className="mcp-picker">
				<Button
					type="button"
					variant="ghost"
					className="mcp-picker-toggle"
					onClick={() => setOpen(!open)}
					aria-expanded={open}
					aria-controls={open ? panelId : undefined}
					aria-describedby={total.tools > 0 ? safetyDescriptionId : undefined}
					disabled={disabled}
				>
					<Plug className="setup-disclosure-icon" aria-hidden />
					<span className="setup-disclosure-title">MCP tools</span>
					<span className="setup-disclosure-summary">
						<span>{total.tools === 0 ? "No tools selected" : selectedToolsLabel(total.tools)}</span>
						{total.tools > 0
							? <span className="mcp-token-count">{tokenCountLabel(total.contextTokens)}</span>
							: null}
						{total.tools > 0 ? (
							<span className="mcp-picker-summary-flags">
								<UsersRound aria-hidden />
								<TriangleAlert aria-hidden />
							</span>
						) : null}
					</span>
					{open
						? <ChevronDown className="setup-disclosure-chevron" aria-hidden />
						: <ChevronRight className="setup-disclosure-chevron" aria-hidden />}
				</Button>
				{total.tools > 0 ? (
					<span className="sr-only" id={safetyDescriptionId}>
						Shared with all agents. May cause remote side effects.
					</span>
				) : null}
				<span className="sr-only" role="status" aria-live="polite" aria-atomic="true">
					{selectionStatus}
				</span>
				{open ? (
					<div className="mcp-picker-list" id={panelId}>
						{authMutationBlockedReason ? (
							<div className="mcp-picker-auth-note mcp-picker-list-note" role="status">
								<Unplug aria-hidden />
								<span>{authMutationBlockedReason}</span>
							</div>
						) : null}
						{serverIds.map((serverId, serverIndex) => {
							const server = inventoryByServer.get(serverId) ?? missingInventoryServer(serverId);
							const auth = authByServer.get(serverId);
							const state = serverSelectionState(pickerInventory, selection, server.server);
							const isExpanded = expanded.has(server.server);
							const selected = selection.get(server.server);
							const selectionReady =
								!authStatusRequired ||
								(authStatusReady &&
									!!auth &&
									(auth.auth_kind !== "oauth" || auth.auth_state === "ready"));
							const locked = lockedSelection.get(server.server);
							const hasEditableTools = server.tools.some((tool) => !locked?.has(tool.raw_name));
							const canToggleServer =
								hasEditableTools &&
								(!!selected?.size ||
									(selectionReady && inventoryReady && server.health === "healthy"));
							const contextTokens = server.tools
								.filter((tool) => selected?.has(tool.raw_name))
								.reduce((sum, tool) => sum + tool.context_token_estimate, 0);
							const selectedCount = server.tools.filter((tool) =>
								selected?.has(tool.raw_name)
							).length;
							const toolsPanelId = `${idPrefix}-mcp-server-${serverIndex}-tools`;
							const serverCheckboxId = `${idPrefix}-mcp-server-${serverIndex}-checkbox`;
							const serverIsBusy = authBusyServer === server.server;
							const authActionDisabled =
								disabled ||
								authBusyServer !== null ||
								!!authMutationBlockedReason;
							const clearAndLogout = () => {
								if (
									selected?.size &&
									!window.confirm(
										`Continue and clear ${server.server}'s selected draft tools?`,
									)
								) return;
								onLogout?.(server.server);
							};
							return (
								<div className="mcp-picker-server" key={server.server}>
									<div className="mcp-picker-server-row">
										<div className="mcp-picker-port">
											{server.tools.length > 0 ? (
												<Button
													type="button"
													variant="ghost"
													size="icon"
													className="mcp-picker-expand size-11"
													onClick={() => toggleExpanded(server.server)}
													aria-expanded={isExpanded}
													aria-controls={isExpanded ? toolsPanelId : undefined}
													aria-label={`${isExpanded ? "collapse" : "expand"} ${server.server} tools`}
													disabled={disabled}
												>
													{isExpanded ? <ChevronDown aria-hidden /> : <ChevronRight aria-hidden />}
												</Button>
											) : (
												<ServerCog className="mcp-picker-port-icon" aria-hidden />
											)}
											{server.tools.length > 0 ? (
												<label
													className="mcp-picker-server-check-target"
													htmlFor={serverCheckboxId}
													title={`Select all tools from ${server.server}`}
												>
													<Checkbox
														id={serverCheckboxId}
														checked={state === "some" ? "indeterminate" : state === "all"}
														aria-label={server.server}
														disabled={disabled || !canToggleServer}
														onCheckedChange={() =>
															onChange(
																selectionReady &&
																		inventoryReady &&
																		server.health === "healthy"
																	? toggleServer(
																			pickerInventory,
																			selection,
																			server.server,
																			lockedSelection,
																		)
																	: clearMcpServerSelection(
																			selection,
																			server.server,
																			lockedSelection,
																		),
															)}
													/>
												</label>
											) : null}
										</div>
										<div className="mcp-picker-server-copy">
											<div className="mcp-picker-server-heading">
												<ServerStatus health={server.health} auth={auth} />
												<span className="mcp-picker-server-name" title={server.server}>
													{server.server}
												</span>
												<span className="mcp-picker-meta">
													(
													{selectedCount > 0
														? serverSelectionLabel(selectedCount, server.tools.length)
														: availableToolsLabel(server.tools.length)}
													{selectedCount > 0
														? <> · {tokenCountLabel(contextTokens)}</>
														: null}
													)
												</span>
											</div>
										</div>
										<div className="mcp-picker-auth-actions">
											{auth?.auth_kind === "oauth" && auth.can_login ? (
												<Button
													type="button"
													variant="outline"
													className="mcp-picker-auth-action"
													onClick={() => onLogin?.(server.server)}
													disabled={authActionDisabled}
												>
													<LogIn data-icon="inline-start" aria-hidden />
													Login
												</Button>
											) : null}
											{auth?.auth_kind === "oauth" &&
											auth.auth_state !== "authorization_pending" &&
											auth.can_logout ? (
												<Button
													type="button"
													variant="ghost"
													className="mcp-picker-auth-action"
													onClick={clearAndLogout}
													disabled={authActionDisabled}
												>
													<LogOut data-icon="inline-start" aria-hidden />
													Logout
												</Button>
											) : null}
											{auth?.auth_kind === "oauth" &&
											auth.auth_state === "authorization_pending" &&
											auth.can_logout ? (
												<Button
													type="button"
													variant="ghost"
													className="mcp-picker-auth-action"
													onClick={clearAndLogout}
													disabled={authActionDisabled}
												>
													<X data-icon="inline-start" aria-hidden />
													Cancel
												</Button>
											) : null}
										</div>
									</div>
									{serverIsBusy ? (
										<div className="mcp-picker-auth-note" role="status">
											<Spinner aria-hidden />
											<span>Working</span>
										</div>
									) : null}
									{auth?.auth_state === "authorization_pending" ? (
										<>
											<div className="mcp-picker-pending">
												<RotateCcw aria-hidden />
												<span>Restart after reload</span>
											</div>
											<span className="sr-only" role="status" aria-live="polite" aria-atomic="true">
												MCP authorization pending. After page reload, cancel and restart.
											</span>
										</>
									) : null}
									{isExpanded && server.tools.length > 0 ? (
										<div className="mcp-picker-tools" id={toolsPanelId}>
											{server.tools.map((tool) => {
												const toolId = `${idPrefix}-mcp-server-${serverIndex}-${tool.raw_name}`;
												const isSelected = selected?.has(tool.raw_name) ?? false;
												const isLocked = locked?.has(tool.raw_name) ?? false;
												return (
													<label className="mcp-picker-tool" htmlFor={toolId} key={tool.raw_name}>
														<Checkbox
															id={toolId}
															checked={isSelected}
															disabled={
																disabled ||
																isLocked ||
																(!selectionReady && !isSelected) ||
																(!inventoryReady && !isSelected) ||
																(server.health !== "healthy" && !isSelected)
															}
															onCheckedChange={() =>
																onChange(
																	toggleTool(
																		selection,
																		server.server,
																		tool.raw_name,
																		lockedSelection,
																	),
																)}
														/>
														<span className="mcp-picker-tool-copy">
															<strong title={tool.raw_name}>{tool.raw_name}</strong>
															{tool.description ? <small>{tool.description}</small> : null}
														</span>
														<span className="mcp-picker-tool-meta">
															{isLocked ? (
																<Badge variant="outline">
																	<LockKeyhole data-icon="inline-start" aria-hidden />
																	Added
																</Badge>
															) : null}
															<span className="mcp-token-count">
																{tokenCountLabel(tool.context_token_estimate)}
															</span>
														</span>
													</label>
												);
											})}
										</div>
									) : null}
								</div>
							);
						})}
					</div>
				) : null}
			</div>
	);
});

function ServerStatus({
	health,
	auth,
}: {
	health: McpInventoryServer["health"];
	auth?: McpAuthServerStatus;
}) {
	const status = serverStatusPresentation(health, auth);
	return (
		<span
			className="mcp-picker-status-dot"
			data-state={status.state}
			role="img"
			aria-label={status.label}
			title={status.detail}
		/>
	);
}

function serverStatusPresentation(
	health: McpInventoryServer["health"],
	auth?: McpAuthServerStatus,
): { state: "online" | "attention" | "offline"; label: string; detail: string } {
	if (health === "unavailable") {
		const detail = auth ? authStatusDetail(auth) : null;
		return {
			state: "offline",
			label: detail ? `Offline, ${detail}` : "Offline",
			detail: detail ? `MCP server is offline; ${detail}` : "MCP server is offline",
		};
	}
	if (health === "revoked") {
		return {
			state: "offline",
			label: "Revoked",
			detail: "MCP server access was revoked",
		};
	}
	const detail = auth ? authStatusDetail(auth) : null;
	const authNeedsAttention =
		auth?.auth_kind === "oauth" &&
		auth.auth_state !== "ready" &&
		auth.auth_state !== "not_applicable";
	return {
		state: authNeedsAttention ? "attention" : "online",
		label: detail ? `Online, ${detail}` : "Online",
		detail: detail ? `MCP server is online; ${detail}` : "MCP server is online",
	};
}

function authStatusDetail(status: McpAuthServerStatus): string {
	if (status.auth_kind === "none") return "no authentication required";
	if (status.auth_kind === "bearer") return "bearer token";
	switch (status.auth_state) {
		case "ready": return "logged in";
		case "login_required": return "login required";
		case "reauthentication_required": return "login expired";
		case "authorization_pending": return "login pending";
		case "unsupported": return "login unsupported";
		case "unknown":
			return status.failure
				? `login status unknown: ${authFailureLabel(status.failure)}`
				: "login status unknown";
		case "not_applicable": return "no login required";
	}
}

function selectedToolsLabel(count: number): string {
	return `${count} ${count === 1 ? "tool" : "tools"} selected`;
}

function serverSelectionLabel(selected: number, available: number): string {
	if (selected === available) return `${selected} selected`;
	return `${selected} / ${available} selected`;
}

function contextTokensLabel(count: number): string {
	return `${count.toLocaleString()} context ${count === 1 ? "token" : "tokens"}`;
}

function tokenCountLabel(count: number): string {
	return `${count.toLocaleString()} ${count === 1 ? "token" : "tokens"}`;
}

function contextBudgetLabel(count: number): string {
	return `About ${contextTokensLabel(count)}`;
}

function mcpSelectionStatus(tools: number, contextTokens: number): string {
	if (tools === 0) return "MCP tool selection: No tools selected.";
	return `MCP tool selection: ${selectedToolsLabel(tools)}. ${contextBudgetLabel(contextTokens)}.`;
}

function availableToolsLabel(count: number): string {
	if (count === 0) return "No tools";
	return `${count} ${count === 1 ? "tool" : "tools"}`;
}

function missingInventoryServer(server: string): McpInventoryServer {
	return { server, revision: "", health: "unavailable", tools: [] };
}

function authFailureLabel(failure: NonNullable<McpAuthServerStatus["failure"]>): string {
	switch (failure) {
		case "credential_store_unavailable": return "OAuth credential storage is unavailable";
		case "discovery_failed": return "OAuth discovery failed";
	}
}
