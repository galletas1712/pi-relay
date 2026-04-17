import type { ExtensionFactory } from "@pi-relay/coding-agent";
import { buildAgentSelectorOptions, buildAgentWidgetLines } from "./roster.js";
import { buildAgentSystemPrompt } from "./system-prompt.js";
import type { Orchestrator } from "./orchestrator.js";

export function createOrchestratorExtension(
	orchestratorRef: { current?: Orchestrator },
	uiRef: { cleanup?: () => void; sessionId?: string } = {},
): ExtensionFactory {
	return (pi) => {
		pi.registerCommand("agents", {
			description: "Show relay agents and switch the active TUI attachment",
			getArgumentCompletions: (prefix) => {
				const orchestrator = orchestratorRef.current;
				if (!orchestrator) {
					return null;
				}

				const lowerPrefix = prefix.trim().toLowerCase();
				const completions = orchestrator
					.getAgentSummaries()
					.filter((summary) => summary.id.toLowerCase().startsWith(lowerPrefix))
					.map((summary) => ({
						value: summary.id,
						label: `${summary.id} (${summary.status})`,
						description: summary.role,
					}));

				return completions.length > 0 ? completions : null;
			},
			handler: async (args, ctx) => {
				const orchestrator = orchestratorRef.current;
				if (!orchestrator) {
					ctx.ui.notify("Relay orchestrator is not available yet.", "warning");
					return;
				}

				const currentAgentId =
					orchestrator.getAgentIdBySessionId(ctx.sessionManager.getSessionId()) ?? orchestrator.rootAgentId;
				let targetAgentId = args.trim();

				if (!targetAgentId) {
					const options = buildAgentSelectorOptions(orchestrator, currentAgentId);
					const labels = options.map((option) => option.label);
					const selected = await ctx.ui.select("Relay Agents", labels);
					if (!selected) {
						return;
					}

					const selectedOption = options.find((option) => option.label === selected);
					if (!selectedOption) {
						return;
					}
					targetAgentId = selectedOption.agentId;
				}

				if (targetAgentId === currentAgentId) {
					ctx.ui.notify(`Already attached to ${targetAgentId}.`, "info");
					return;
				}

				const summary = orchestrator.getAgentSummaries().find((entry) => entry.id === targetAgentId);
				if (!summary) {
					ctx.ui.notify(`Unknown agent: ${targetAgentId}`, "error");
					return;
				}
				if (!summary.sessionFile) {
					ctx.ui.notify(`Agent ${targetAgentId} does not have a persisted session file.`, "error");
					return;
				}

				const result = await ctx.switchSession(summary.sessionFile);
				if (result.cancelled) {
					return;
				}
				ctx.ui.notify(`Attached to ${summary.id} (${summary.role}).`, "info");
			},
		});

		pi.on("session_start", async (_event, ctx) => {
			const orchestrator = orchestratorRef.current;
			if (!orchestrator) {
				return;
			}

			if (ctx.hasUI) {
				uiRef.cleanup?.();
				uiRef.cleanup = undefined;
				uiRef.sessionId = ctx.sessionManager.getSessionId();

				const updateWidget = () => {
					if (uiRef.sessionId !== ctx.sessionManager.getSessionId()) {
						return;
					}

					const activeAgentId =
						orchestrator.getAgentIdBySessionId(ctx.sessionManager.getSessionId()) ?? orchestrator.rootAgentId;
					const widgetLines = buildAgentWidgetLines(orchestrator, activeAgentId);
					if (!widgetLines) {
						ctx.ui.setWidget("relay-agents", undefined);
						return;
					}

					ctx.ui.setWidget("relay-agents", widgetLines, { placement: "belowEditor" });
				};

				updateWidget();
				uiRef.cleanup = orchestrator.subscribeToChanges(updateWidget);
			}
		});

		pi.on("before_agent_start", async (event, ctx) => {
			const orchestrator = orchestratorRef.current;
			if (!orchestrator) {
				return;
			}

			const agentId = orchestrator.getAgentIdBySessionId(ctx.sessionManager.getSessionId());
			if (!agentId) {
				return;
			}

			const record = orchestrator.getRecord(agentId);
			return {
				systemPrompt: buildAgentSystemPrompt(event.systemPrompt, {
					role: record.role,
					hasParent: record.parentId !== null,
				}),
			};
		});

		pi.on("session_shutdown", async (_event, ctx) => {
			const orchestrator = orchestratorRef.current;
			if (uiRef.sessionId === ctx.sessionManager.getSessionId()) {
				uiRef.cleanup?.();
				uiRef.cleanup = undefined;
				uiRef.sessionId = undefined;
			}
			if (!orchestrator || orchestrator.isDisposing) {
				return;
			}

			const agentId = orchestrator.getAgentIdBySessionId(ctx.sessionManager.getSessionId());
			if (agentId !== orchestrator.rootAgentId) {
				return;
			}

			await orchestrator.dispose();
		});
	};
}
