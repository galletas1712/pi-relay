import type { AgentRecord } from "./types.js";
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
