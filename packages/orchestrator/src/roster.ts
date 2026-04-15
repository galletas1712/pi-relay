import type { AgentRecord, AgentSummary } from "./types.js";
import type { Orchestrator } from "./orchestrator.js";

const GREEN_BULLET = "\u001b[32m●\u001b[39m";

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

function formatDirectChildLine(summary: AgentSummary): string {
	const details: string[] = [summary.displayStatus];
	if (summary.childCount > 0) {
		details.push(`${summary.childCount} child${summary.childCount === 1 ? "" : "ren"}`);
	}
	return `- ${summary.id} (${details.join(", ")}): ${summary.role}`;
}

export function buildDirectChildRoster(orchestrator: Orchestrator, agentId: string): string {
	const children = orchestrator.getDirectChildSummaries(agentId);
	if (children.length === 0) {
		return "You have no direct children.";
	}

	const active = children.filter((c) => c.displayStatus !== "idle");
	const idle = children.filter((c) => c.displayStatus === "idle");
	const lines: string[] = [];

	if (active.length > 0) {
		lines.push("## Active Children", "");
		for (const child of active) {
			lines.push(formatDirectChildLine(child));
		}
	}

	if (idle.length > 0) {
		if (lines.length > 0) lines.push("");
		lines.push("## Idle Children", "");
		for (const child of idle) {
			lines.push(formatDirectChildLine(child));
		}
	}

	return lines.join("\n");
}

function formatAgentMarker(summary: AgentSummary): string {
	return summary.displayStatus === "idle" ? " " : GREEN_BULLET;
}

function formatStatusNote(summary: AgentSummary): string | undefined {
	if (summary.displayStatus === "waiting" || summary.displayStatus === "starting") {
		return summary.displayStatus;
	}
	return undefined;
}

function formatChildCount(summary: AgentSummary): string | undefined {
	if (summary.childCount === 0) {
		return undefined;
	}
	return `${summary.childCount} child${summary.childCount === 1 ? "" : "ren"}`;
}

export function formatAgentDisplayName(summary: AgentSummary): string {
	const statusNote = formatStatusNote(summary);
	const suffix = statusNote ? ` (${statusNote})` : "";
	return `${formatAgentMarker(summary)} ${summary.id}${suffix}`;
}

export function formatAgentCompletionLabel(summary: AgentSummary): string {
	return `${formatAgentDisplayName(summary)} · ${summary.role}`;
}

export function buildAgentCompletionLabel(summary: AgentSummary): string {
	return formatAgentCompletionLabel(summary);
}

export function buildAgentSelectorOptions(
	orchestrator: Orchestrator,
	activeAgentId: string,
): Array<{ agentId: string; label: string }> {
	return orchestrator.getAgentSummaries().map((summary) => {
		const prefix = `${"  ".repeat(summary.depth)}${summary.id === activeAgentId ? "* " : ""}`;
		const label = `${prefix}${formatAgentDisplayName(summary)} · ${summary.role} — ${truncate(summary.lastOutput, 80)}`;
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
	const visible = summaries.filter((summary) => summary.id === activeAgentId || summary.displayStatus !== "idle");
	if (visible.length === 1 && active.id === "root") {
		return undefined;
	}
	const shown = visible.slice(0, maxAgents);
	const hiddenIdleCount = summaries.length - visible.length;

	const lines = [
		"Relay Agents",
		`Attached: ${formatAgentDisplayName(active)} (${active.role})`,
		"Other agents keep running detached when not attached.",
	];

	for (const summary of shown) {
		const marker = summary.id === activeAgentId ? ">" : " ";
		const details = [summary.role, formatStatusNote(summary), formatChildCount(summary)].filter((part) => part !== undefined);
		lines.push(`${marker} ${"  ".repeat(summary.depth)}${formatAgentDisplayName(summary)} · ${details.join(" · ")}`);
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
