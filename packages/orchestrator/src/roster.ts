import type { AgentRecord, AgentSummary } from "./types.js";
import type { Orchestrator } from "./orchestrator.js";

function truncate(text: string | undefined, maxLength: number): string {
	if (!text) {
		return "(no activity yet)";
	}

	const normalized = text.replace(/\s+/g, " ").trim();
	if (normalized.length <= maxLength) {
		return normalized;
	}
	return `${normalized.slice(0, maxLength - 3)}...`;
}

function formatChildLine(record: AgentRecord): string {
	const suffix =
		record.childIds.length > 0
			? `, ${record.childIds.length} child${record.childIds.length === 1 ? "" : "ren"}`
			: "";
	return `- ${record.id} (${record.status}${suffix}): ${record.role}`;
}

function getDisplayStatus(orchestrator: Orchestrator, summary: AgentSummary): AgentSummary["status"] {
	try {
		const record = orchestrator.getRecord(summary.id);
		if (
			record.session.isStreaming ||
			record.session.isRetrying ||
			record.session.isCompacting ||
			record.reactivating
		) {
			return "running";
		}
		if (record.status === "disposed") {
			return "disposed";
		}
		return "idle";
	} catch {
		return summary.status;
	}
}

export function buildSubagentRoster(orchestrator: Orchestrator, agentId: string): string {
	const children = orchestrator.getChildrenOf(agentId).filter((child) => child.status === "running");
	if (children.length === 0) {
		return "";
	}

	const lines = ["## Running Subagents", ""];
	for (const child of children) {
		lines.push(formatChildLine(child));
	}
	return lines.join("\n");
}

function formatSummaryStatus(orchestrator: Orchestrator, summary: AgentSummary): string {
	const displayStatus = getDisplayStatus(orchestrator, summary);
	if (summary.childCount === 0) {
		return displayStatus;
	}
	return `${displayStatus}, ${summary.childCount} child${summary.childCount === 1 ? "" : "ren"}`;
}

export function buildAgentSelectorOptions(
	orchestrator: Orchestrator,
	activeAgentId: string,
): Array<{ agentId: string; label: string }> {
	return orchestrator.getAgentSummaries().map((summary) => {
		const prefix = `${"  ".repeat(summary.depth)}${summary.id === activeAgentId ? "* " : ""}`;
		const label = `${prefix}${summary.id} [${getDisplayStatus(orchestrator, summary)}] ${summary.role} — ${truncate(summary.lastOutput, 80)}`;
		return {
			agentId: summary.id,
			label,
		};
	});
}

export function buildAgentWidgetLines(
	orchestrator: Orchestrator,
	activeAgentId: string,
	maxAgents = 8,
): string[] | undefined {
	const summaries = orchestrator.getAgentSummaries();
	if (summaries.length <= 1) {
		return undefined;
	}
	const active = summaries.find((summary) => summary.id === activeAgentId) ?? summaries[0];
	if (!active) {
		return undefined;
	}
	const visible = summaries.filter((summary) => summary.id === activeAgentId || summary.status === "running");
	if (visible.length === 1 && active.id === "root") {
		return undefined;
	}
	const shown = visible.slice(0, maxAgents);
	const hiddenIdleCount = summaries.length - visible.length;
	const activeDisplayStatus = getDisplayStatus(orchestrator, active);

	const lines = [
		"Relay Agents",
		`Attached: ${active.id} (${active.role}, ${activeDisplayStatus})`,
		"Other agents keep running detached when not attached.",
	];

	for (const summary of shown) {
		const marker = summary.id === activeAgentId ? ">" : " ";
		lines.push(
			`${marker} ${"  ".repeat(summary.depth)}${summary.id} · ${formatSummaryStatus(orchestrator, summary)} · ${summary.role}`,
		);
	}

	if (visible.length > maxAgents) {
		lines.push(`… ${visible.length - maxAgents} more active agents`);
	}

	if (hiddenIdleCount > 0) {
		lines.push(`… ${hiddenIdleCount} idle agent${hiddenIdleCount === 1 ? "" : "s"} hidden`);
	}

	lines.push("Use /agents to switch");
	return lines;
}
