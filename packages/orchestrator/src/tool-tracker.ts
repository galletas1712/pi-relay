import type { ToolCallRecord } from "./types.js";

export class ToolCallTracker {
	private readonly records = new Map<string, ToolCallRecord>();

	register(agentId: string, toolCallId: string, toolName: string): void {
		this.records.set(toolCallId, {
			toolCallId,
			agentId,
			toolName,
			startedAt: Date.now(),
			status: "running",
		});
	}

	attachAbortController(toolCallId: string, abortController: AbortController): void {
		const record = this.records.get(toolCallId);
		if (!record) {
			return;
		}
		record.abortController = abortController;
	}

	complete(toolCallId: string, status: ToolCallRecord["status"] = "completed"): void {
		const record = this.records.get(toolCallId);
		if (!record) {
			return;
		}
		record.status = status;
		this.records.delete(toolCallId);
	}

	killAllForAgent(agentId: string): void {
		for (const [toolCallId, record] of this.records) {
			if (record.agentId !== agentId) {
				continue;
			}
			record.abortController?.abort();
			this.records.delete(toolCallId);
		}
	}

	getInFlightForAgent(agentId: string): ToolCallRecord[] {
		return [...this.records.values()].filter((record) => record.agentId === agentId);
	}
}
