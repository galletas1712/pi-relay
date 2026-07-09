import { memo, useEffect, useRef, useState } from "react";
import { ChevronDown, ChevronRight } from "lucide-react";
import {
	mcpSelectionTotals,
	serverSelectionState,
	toggleServer,
	toggleTool,
	type McpSelectionState,
} from "./mcpSelection.ts";
import type { McpInventory } from "./types.ts";

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
}: {
	inventory: McpInventory;
	selection: McpSelectionState;
	onChange: (selection: McpSelectionState) => void;
	disabled?: boolean;
	inventoryReady?: boolean;
	open?: boolean;
	onOpenChange?: (open: boolean) => void;
}) {
	const [internalOpen, setInternalOpen] = useState(false);
	const open = controlledOpen ?? internalOpen;
	const [expanded, setExpanded] = useState<ReadonlySet<string>>(new Set());
	if (!inventory.servers.length) return null;
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
					{inventory.servers.map((server) => {
						const state = serverSelectionState(inventory, selection, server.server);
						const isExpanded = expanded.has(server.server);
						const selected = selection.get(server.server);
						const canToggleServer =
							server.tools.length > 0 &&
							(inventoryReady || state === "all");
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
													!canToggleServer ||
													(server.health !== "healthy" && !selected?.size)
												}
												onChange={() => onChange(toggleServer(inventory, selection, server.server))}
											/>
										) : null}
										<span>{server.server}</span>
									</label>
									<span className={`mcp-picker-health ${server.health}`}>{server.health}</span>
									<span className="mcp-picker-meta">
										{selected?.size ?? 0}/{server.tools.length} tools · ≈{contextTokens.toLocaleString()} tokens
									</span>
								</div>
								{isExpanded ? (
									<div className="mcp-picker-tools">
										{server.tools.map((tool) => (
											<label className="mcp-picker-tool" key={tool.raw_name}>
												<input
													type="checkbox"
													checked={selected?.has(tool.raw_name) ?? false}
													disabled={
														disabled ||
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
