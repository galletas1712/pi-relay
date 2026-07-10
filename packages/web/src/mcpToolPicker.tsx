import { memo, useEffect, useRef, useState } from "react";
import { ChevronDown, ChevronRight } from "lucide-react";
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
	const total = mcpSelectionTotals(inventory, selection);
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
				disabled={disabled}
			>
				{open ? <ChevronDown size={14} /> : <ChevronRight size={14} />}
				<span>MCP tools</span>
				<span className="mcp-picker-count">
					{total.tools} selected · ≈{total.contextTokens.toLocaleString()} MCP context tokens added
				</span>
			</button>
			{total.tools > 0 ? (
				<p className="mcp-picker-warning">
					All full and read-only subagents inherit these tools. Read-only restricts local files only; MCP tools may cause remote side effects.
				</p>
			) : null}
			{open ? (
				<div className="mcp-picker-list">
					{serverIds.map((serverId) => {
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
						return (
							<div className="mcp-picker-server" key={server.server}>
								<div className="mcp-picker-server-row">
									<button
										type="button"
										className="mcp-picker-expand"
										onClick={() => toggleExpanded(server.server)}
										aria-expanded={isExpanded}
										aria-label={`${isExpanded ? "collapse" : "expand"} ${server.server} tools`}
										disabled={disabled}
									>
										{isExpanded ? <ChevronDown size={14} /> : <ChevronRight size={14} />}
									</button>
									<label className="mcp-picker-server-name">
										{server.tools.length > 0 ? (
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
										) : null}
										<span>{server.server}</span>
									</label>
									{auth ? (
										<span
											className={`mcp-picker-auth ${auth.auth_state}`}
										>
											{authLabel(auth)}
											{auth.failure ? ` · ${authFailureLabel(auth.failure)}` : ""}
										</span>
									) : null}
									<span className={`mcp-picker-health ${server.health}`}>{server.health}</span>
									<span className="mcp-picker-meta">
										{selected?.size ?? 0}/{server.tools.length} tools · ≈{contextTokens.toLocaleString()} tokens
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
									<p className="mcp-picker-pending" role="status">
										Authorization is pending. If this page was reloaded, cancel it and start again.
									</p>
								) : null}
								{isExpanded ? (
									<div className="mcp-picker-tools">
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
												<small>≈{tool.context_token_estimate.toLocaleString()} tokens</small>
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

function missingInventoryServer(server: string): McpInventoryServer {
	return { server, revision: "", health: "unavailable", tools: [] };
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
