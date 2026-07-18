import { memo, useEffect, useId, useRef, useState } from "react";
import { ChevronDown, ChevronRight, Plug } from "lucide-react";
import {
	mcpSelectionTotals,
	clearMcpServerSelection,
	serverSelectionState,
	toggleServer,
	toggleTool,
	type McpSelectionState,
} from "./mcpSelection.ts";
import type { McpAuthServerStatus, McpInventory, McpInventoryServer } from "./types.ts";

function MixedCheckbox({
	checked,
	mixed,
	...props
}: Omit<React.InputHTMLAttributes<HTMLInputElement>, "type"> & { mixed: boolean }) {
	const ref = useRef<HTMLInputElement>(null);
	useEffect(() => {
		if (ref.current) ref.current.indeterminate = mixed;
	}, [mixed]);
	return <input ref={ref} type="checkbox" checked={checked} aria-checked={mixed ? "mixed" : checked} {...props} />;
}

export const McpToolPicker = memo(function McpToolPicker({
	inventory,
	selection,
	onChange,
	disabled,
	inventoryReady = true,
	open: controlledOpen,
	onOpenChange,
	authStatus = [],
	authStatusReady = true,
	onLogin,
	onLogout,
	authBusyServer = null,
	authMutationBlockedReason = null,
}: {
	inventory: McpInventory;
	selection: McpSelectionState;
	onChange: (selection: McpSelectionState) => void;
	disabled?: boolean;
	inventoryReady?: boolean;
	open?: boolean;
	onOpenChange?: (open: boolean) => void;
	authStatus?: McpAuthServerStatus[];
	authStatusReady?: boolean;
	onLogin?: (server: string) => void;
	onLogout?: (server: string) => void;
	authBusyServer?: string | null;
	authMutationBlockedReason?: string | null;
}) {
	const idPrefix = useId();
	const [internalOpen, setInternalOpen] = useState(false);
	const open = controlledOpen ?? internalOpen;
	const [expanded, setExpanded] = useState<ReadonlySet<string>>(new Set());
	const inventoryByServer = new Map(inventory.servers.map((server) => [server.server, server]));
	const authByServer = new Map(authStatus.map((status) => [status.server, status]));
	const serverIds = [...new Set([
		...authStatus.map((status) => status.server),
		...inventory.servers.map((server) => server.server),
	])].sort();
	if (!serverIds.length) return null;
	const panelId = `${idPrefix}-mcp-panel`;
	const total = mcpSelectionTotals(inventory, selection);
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
			<button
				type="button"
				className="mcp-picker-toggle"
				onClick={() => setOpen(!open)}
				aria-expanded={open}
				aria-controls={open ? panelId : undefined}
				disabled={disabled}
			>
				<Plug className="setup-disclosure-icon" size={18} aria-hidden />
				<span className="setup-disclosure-title">MCP tools</span>
				<span className="setup-disclosure-summary">
					{total.tools === 0 ? (
						<span>No tools selected</span>
					) : (
						<>
							<span>{selectedToolsLabel(total.tools)}</span>
							<span>{contextBudgetLabel(total.contextTokens)}</span>
						</>
					)}
				</span>
				{open
					? <ChevronDown className="setup-disclosure-chevron" size={16} aria-hidden />
					: <ChevronRight className="setup-disclosure-chevron" size={16} aria-hidden />}
			</button>
			<span className="sr-only" role="status" aria-live="polite" aria-atomic="true">
				{selectionStatus}
			</span>
			{total.tools > 0 ? (
				<dl className="mcp-picker-safety" aria-label="MCP tool access">
					<div>
						<dt>Scope</dt>
						<dd>All agents</dd>
					</div>
					<div>
						<dt>Risk</dt>
						<dd>Remote side effects</dd>
					</div>
				</dl>
			) : null}
			{open ? (
				<div className="mcp-picker-list" id={panelId}>
					{serverIds.map((serverId, serverIndex) => {
						const server = inventoryByServer.get(serverId) ?? missingInventoryServer(serverId);
						const auth = authByServer.get(serverId);
						const state = serverSelectionState(inventory, selection, server.server);
						const isExpanded = expanded.has(server.server);
						const selected = selection.get(server.server);
						const selectionReady =
							authStatusReady &&
							!!auth &&
							(auth.auth_kind !== "oauth" || auth.auth_state === "ready");
						const canToggleServer =
							server.tools.length > 0 &&
							(!!selected?.size ||
								(selectionReady && inventoryReady && server.health === "healthy"));
						const contextTokens = server.tools
							.filter((tool) => selected?.has(tool.raw_name))
							.reduce((sum, tool) => sum + tool.context_token_estimate, 0);
						const selectedCount = server.tools.filter((tool) =>
							selected?.has(tool.raw_name)
						).length;
						const toolsPanelId = `${idPrefix}-mcp-server-${serverIndex}-tools`;
						return (
							<div className="mcp-picker-server" key={server.server}>
								<div className="mcp-picker-server-row">
									{server.tools.length > 0 ? (
										<button
											type="button"
											className="mcp-picker-expand"
											onClick={() => toggleExpanded(server.server)}
											aria-expanded={isExpanded}
											aria-controls={isExpanded ? toolsPanelId : undefined}
											aria-label={`${isExpanded ? "collapse" : "expand"} ${server.server} tools`}
											disabled={disabled}
										>
											{isExpanded
												? <ChevronDown size={14} aria-hidden />
												: <ChevronRight size={14} aria-hidden />}
										</button>
									) : null}
									{server.tools.length > 0 ? (
										<label className="mcp-picker-server-name">
											<MixedCheckbox
												checked={state === "all"}
												mixed={state === "some"}
												disabled={
													disabled ||
													!canToggleServer
												}
												onChange={() =>
													onChange(
														selectionReady &&
															inventoryReady &&
															server.health === "healthy"
															? toggleServer(inventory, selection, server.server)
															: clearMcpServerSelection(selection, server.server),
													)}
											/>
											<span>{server.server}</span>
										</label>
									) : (
										<span className="mcp-picker-server-name">
											<span>{server.server}</span>
										</span>
									)}
									{auth ? (
										<span
											className={`mcp-picker-auth ${auth.auth_state}`}
										>
											<span>{authLabel(auth)}</span>
											{auth.failure
												? <span>{authFailureLabel(auth.failure)}</span>
												: null}
										</span>
									) : null}
									<span className={`mcp-picker-health ${server.health}`}>
										{healthLabel(server.health)}
									</span>
									<span className="mcp-picker-meta">
										{selectedCount > 0 ? (
											<>
												<span>{serverSelectionLabel(selectedCount, server.tools.length)}</span>
												<span>{contextBudgetLabel(contextTokens)}</span>
											</>
										) : (
											<span>{availableToolsLabel(server.tools.length)}</span>
										)}
									</span>
									{auth?.auth_kind === "oauth" && auth.can_login ? (
										<button
											type="button"
											className="mcp-picker-auth-action"
											onClick={() => onLogin?.(server.server)}
											disabled={
												disabled ||
												authBusyServer !== null ||
												!!authMutationBlockedReason
											}
											title={authMutationBlockedReason ?? undefined}
										>
											{authBusyServer === server.server ? "Starting…" : "Login"}
										</button>
									) : null}
									{auth?.auth_kind === "oauth" &&
									auth.auth_state !== "authorization_pending" &&
									auth.can_logout ? (
										<button
											type="button"
											className="mcp-picker-auth-action"
											onClick={() => {
												if (
													selected?.size &&
													!window.confirm(
														`Continue and clear ${server.server}'s selected draft tools?`,
													)
												) return;
												onLogout?.(server.server);
											}}
											disabled={
												disabled ||
												authBusyServer !== null ||
												!!authMutationBlockedReason
											}
											title={authMutationBlockedReason ?? undefined}
										>
											{authBusyServer === server.server ? "Logging out…" : "Logout"}
										</button>
									) : null}
									{auth?.auth_kind === "oauth" &&
									auth.auth_state === "authorization_pending" &&
									auth.can_logout ? (
										<button
											type="button"
											className="mcp-picker-auth-action"
											onClick={() => {
												if (
													selected?.size &&
													!window.confirm(
														`Continue and clear ${server.server}'s selected draft tools?`,
													)
												) return;
												onLogout?.(server.server);
											}}
											disabled={
												disabled ||
												authBusyServer !== null ||
												!!authMutationBlockedReason
											}
											title={authMutationBlockedReason ?? undefined}
										>
											{authBusyServer === server.server ? "Cancelling…" : "Cancel"}
										</button>
									) : null}
								</div>
								{auth?.auth_state === "authorization_pending" ? (
									<>
										<dl
											className="mcp-picker-pending"
											aria-label="MCP authorization status"
										>
											<div>
												<dt>Status</dt>
												<dd>Authorization pending</dd>
											</div>
											<div>
												<dt>After reload</dt>
												<dd>Cancel and restart</dd>
											</div>
										</dl>
										<span className="sr-only" role="status" aria-live="polite" aria-atomic="true">
											MCP authorization pending. After page reload, cancel and restart.
										</span>
									</>
								) : null}
								{isExpanded && server.tools.length > 0 ? (
									<div className="mcp-picker-tools" id={toolsPanelId}>
										{server.tools.map((tool) => (
											<label className="mcp-picker-tool" key={tool.raw_name}>
												<input
													type="checkbox"
													checked={selected?.has(tool.raw_name) ?? false}
													disabled={
														disabled ||
														(!selectionReady && !selected?.has(tool.raw_name)) ||
														(!inventoryReady && !selected?.has(tool.raw_name)) ||
														(server.health !== "healthy" && !selected?.has(tool.raw_name))
													}
													onChange={() => onChange(toggleTool(selection, server.server, tool.raw_name))}
												/>
												<span>
													<strong>{tool.raw_name}</strong>
													{tool.description ? <small>{tool.description}</small> : null}
												</span>
												<small>About {contextTokensLabel(tool.context_token_estimate)}</small>
											</label>
										))}
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

function selectedToolsLabel(count: number): string {
	return `${count} ${count === 1 ? "tool" : "tools"} selected`;
}

function serverSelectionLabel(selected: number, available: number): string {
	if (selected === available) {
		return selected === 1 ? selectedToolsLabel(selected) : `All ${selected} tools selected`;
	}
	return `${selected} of ${available} tools selected`;
}

function contextTokensLabel(count: number): string {
	return `${count.toLocaleString()} context ${count === 1 ? "token" : "tokens"}`;
}

function contextBudgetLabel(count: number): string {
	return `About ${contextTokensLabel(count)}`;
}

function mcpSelectionStatus(tools: number, contextTokens: number): string {
	if (tools === 0) return "MCP tool selection: No tools selected.";
	return `MCP tool selection: ${selectedToolsLabel(tools)}. ${contextBudgetLabel(contextTokens)}.`;
}

function availableToolsLabel(count: number): string {
	if (count === 0) return "No tools available";
	return `${count} ${count === 1 ? "tool" : "tools"} available`;
}

function missingInventoryServer(server: string): McpInventoryServer {
	return { server, revision: "", health: "unavailable", tools: [] };
}

function healthLabel(health: McpInventoryServer["health"]): string {
	switch (health) {
		case "healthy": return "Healthy";
		case "unavailable": return "Unavailable";
		case "revoked": return "Revoked";
	}
}

function authLabel(status: McpAuthServerStatus): string {
	if (status.auth_kind === "none") return "no auth";
	if (status.auth_kind === "bearer") return "bearer";
	switch (status.auth_state) {
		case "ready": return "OAuth ready";
		case "login_required": return "login required";
		case "reauthentication_required": return "login expired";
		case "authorization_pending": return "login pending";
		case "unsupported": return "OAuth unsupported";
		case "unknown": return "OAuth unknown";
		case "not_applicable": return "OAuth";
	}
}

function authFailureLabel(failure: NonNullable<McpAuthServerStatus["failure"]>): string {
	switch (failure) {
		case "credential_store_unavailable": return "OAuth credential storage is unavailable";
		case "discovery_failed": return "OAuth discovery failed";
	}
}
