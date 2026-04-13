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
	const suffix = record.childIds.length > 0 ? `, ${record.childIds.length} children` : "";
	return `- ${record.id} (${record.status}${suffix}): ${record.role} — ${truncate(record.session.getLastAssistantText(), 100)}`;
}

export function buildSubagentRoster(orchestrator: Orchestrator, agentId: string): string {
	const children = orchestrator.getChildrenOf(agentId);
	if (children.length === 0) {
		return "";
	}

	const lines = ["## Active Subagents", ""];
	for (const child of children) {
		lines.push(formatChildLine(child));
	}
	return lines.join("\n");
}

function formatSummaryStatus(summary: AgentSummary): string {
	if (summary.childCount === 0) {
		return summary.status;
	}
	return `${summary.status}, ${summary.childCount} child${summary.childCount === 1 ? "" : "ren"}`;
}

export function buildAgentSelectorOptions(
	orchestrator: Orchestrator,
	activeAgentId: string,
): Array<{ agentId: string; label: string }> {
	return orchestrator.getAgentSummaries().map((summary) => {
		const prefix = `${"  ".repeat(summary.depth)}${summary.id === activeAgentId ? "* " : ""}`;
		const label = `${prefix}${summary.id} [${summary.status}] ${summary.role} — ${truncate(summary.lastOutput, 80)}`;
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
): string[] {
	const summaries = orchestrator.getAgentSummaries();
	const active = summaries.find((summary) => summary.id === activeAgentId) ?? summaries[0];
	if (!active) {
		return [];
	}

	const lines = [
		"Relay Agents",
		`Attached: ${active.id} (${active.role}, ${active.status})`,
		"Other agents keep running detached when not attached.",
	];

	for (const summary of summaries.slice(0, maxAgents)) {
		const marker = summary.id === activeAgentId ? ">" : " ";
		lines.push(
			`${marker} ${"  ".repeat(summary.depth)}${summary.id} · ${formatSummaryStatus(summary)} · ${summary.role}`,
		);
	}

	if (summaries.length > maxAgents) {
		lines.push(`… ${summaries.length - maxAgents} more agents`);
	}

	lines.push("Use /agents to switch");
	return lines;
}
