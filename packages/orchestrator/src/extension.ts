import type { ExtensionCommandContext, ExtensionFactory, SettingsManager } from "@pi-relay/coding-agent";
import type { ThinkingLevel } from "@pi-relay/agent-core";
import { getThinkingLevels, type Model } from "@pi-relay/ai";
import { buildAgentSelectorOptions, buildAgentWidgetLines } from "./roster.js";
import type { Orchestrator } from "./orchestrator.js";

/**
 * Optional hooks the host supplies so the orchestrator extension can
 * persist the user's `/worklog-model` choice across restarts.
 */
export interface OrchestratorExtensionOptions {
	/**
	 * Returns the active settings manager, or `undefined` if the host does
	 * not wish to persist the fork-model choice (choice will be
	 * session-only).
	 */
	getSettingsManager?: () => SettingsManager | undefined;
}

export function createOrchestratorExtension(
	orchestratorRef: { current?: Orchestrator },
	uiRef: { cleanup?: () => void; sessionId?: string } = {},
	options: OrchestratorExtensionOptions = {},
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

		pi.registerCommand("worklog-model", {
			description:
				"Select the model used for worklog forks (off-transcript per-turn summary call)",
			getArgumentCompletions: async (prefix) => {
				// Argument completions mirror `/model`: fuzzy-ish prefix match on
				// `provider/id`. We can't consult the ctx.modelRegistry here
				// because `getArgumentCompletions` has no ctx argument, so we
				// return null and let the command handler do the picker work.
				const trimmed = prefix.trim().toLowerCase();
				// Always surface `none` as a completion so users can clear the
				// override without opening the picker.
				if ("none".startsWith(trimmed) || trimmed === "") {
					return [
						{
							value: "none",
							label: "none",
							description: "Clear override; fall back to session model",
						},
					];
				}
				return null;
			},
			handler: async (args, ctx) => {
				const orchestrator = orchestratorRef.current;
				if (!orchestrator) {
					ctx.ui.notify("Relay orchestrator is not available yet.", "warning");
					return;
				}

				const settingsManager = options.getSettingsManager?.();
				const trimmed = args.trim();

				// Branch 1: inspect current state.
				if (trimmed === "" && !ctx.hasUI) {
					// Print-mode / RPC: no picker available. Show current value.
					summarizeForkModel(orchestrator, ctx);
					return;
				}

				// Branch 2: explicit clear.
				if (trimmed.toLowerCase() === "none" || trimmed.toLowerCase() === "unset") {
					applyForkModelChoice(orchestrator, undefined, undefined, settingsManager);
					ctx.ui.notify("Worklog fork: cleared override (will use session model).", "info");
					return;
				}

				// Branch 3: explicit "<modelref>" or "<modelref> <level>".
				if (trimmed !== "") {
					const parts = trimmed.split(/\s+/);
					const modelRef = parts[0] ?? "";
					const explicitLevel = parts[1];
					ctx.modelRegistry.refresh();
					const availableModels = await ctx.modelRegistry.getAvailable();
					const match = resolveModelByReference(modelRef, availableModels as Model<any>[]);
					if (!match) {
						if (!ctx.hasUI) {
							ctx.ui.notify(`No model matches '${modelRef}'.`, "error");
							return;
						}
						// UI mode: open the picker pre-filtered by the search term.
						await runForkModelPicker(orchestrator, ctx, settingsManager, modelRef);
						return;
					}
					const resolvedLevel = resolveThinkingLevel(match, explicitLevel);
					applyForkModelChoice(orchestrator, match, resolvedLevel, settingsManager);
					ctx.ui.notify(
						`Worklog fork: ${match.provider}/${match.id}${resolvedLevel ? ` (${resolvedLevel})` : ""}.`,
						"info",
					);
					return;
				}

				// Branch 4: no argument — open interactive picker.
				await runForkModelPicker(orchestrator, ctx, settingsManager, undefined);
			},
		});

		pi.on("session_start", async (_event, ctx) => {
			const orchestrator = orchestratorRef.current;
			if (!orchestrator) {
				return;
			}

			// Register subtree-usage provider for the TUI footer and print-mode
			// telemetry. The callback resolves the attached agent id at each call
			// so the correct subtree is reported as the user switches agents.
			const resolveAttachedAgentId = (): string =>
				orchestrator.getAgentIdBySessionId(ctx.sessionManager.getSessionId()) ?? orchestrator.rootAgentId;
			ctx.setSubtreeUsageProvider(() => {
				try {
					return orchestrator.aggregateSubtreeUsage(resolveAttachedAgentId());
				} catch {
					return undefined;
				}
			});

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


		pi.on("session_shutdown", async (_event, ctx) => {
			const orchestrator = orchestratorRef.current;
			if (uiRef.sessionId === ctx.sessionManager.getSessionId()) {
				uiRef.cleanup?.();
				uiRef.cleanup = undefined;
				uiRef.sessionId = undefined;
			}
			// Clear subtree-usage provider so post-shutdown renders fall back to
			// self-only stats. Safe to call even when the orchestrator is gone.
			ctx.setSubtreeUsageProvider(undefined);
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

/**
 * Resolve a user-supplied model reference (`provider/id` or bare `id`)
 * against the list of currently available models. Mirrors the matching
 * policy used by the main `/model` command: exact canonical match wins;
 * ambiguous bare-id matches return `undefined` rather than guessing.
 */
export function resolveModelByReference(
	reference: string,
	availableModels: Model<any>[],
): Model<any> | undefined {
	const trimmed = reference.trim();
	if (!trimmed) {
		return undefined;
	}
	const normalized = trimmed.toLowerCase();

	const canonical = availableModels.filter(
		(model) => `${model.provider}/${model.id}`.toLowerCase() === normalized,
	);
	if (canonical.length === 1) {
		return canonical[0];
	}
	if (canonical.length > 1) {
		return undefined;
	}

	const slashIndex = trimmed.indexOf("/");
	if (slashIndex !== -1) {
		const provider = trimmed.slice(0, slashIndex).trim().toLowerCase();
		const modelId = trimmed.slice(slashIndex + 1).trim().toLowerCase();
		if (provider && modelId) {
			const providerMatches = availableModels.filter(
				(model) => model.provider.toLowerCase() === provider && model.id.toLowerCase() === modelId,
			);
			if (providerMatches.length === 1) {
				return providerMatches[0];
			}
			if (providerMatches.length > 1) {
				return undefined;
			}
		}
	}

	const idMatches = availableModels.filter((model) => model.id.toLowerCase() === normalized);
	return idMatches.length === 1 ? idMatches[0] : undefined;
}

/**
 * Validate a requested thinking level against the model's supported set.
 * If the user asked for a level the model doesn't support (or didn't
 * specify one), we default to `medium` when available, otherwise the
 * first supported level, otherwise `undefined` (non-reasoning model).
 */
export function resolveThinkingLevel(
	model: Model<any>,
	requested: string | undefined,
): ThinkingLevel | undefined {
	const supported = getThinkingLevels(model);
	if (supported.length === 0) {
		return undefined;
	}
	if (requested) {
		const normalized = requested.toLowerCase();
		const match = supported.find((level) => level.toLowerCase() === normalized);
		if (match) {
			return match as ThinkingLevel;
		}
	}
	if (supported.includes("medium")) {
		return "medium";
	}
	return supported[0] as ThinkingLevel;
}

/**
 * Apply the user's fork-model choice to the orchestrator's live config
 * and, if a SettingsManager is wired in, persist it for future restarts.
 * Passing `undefined` for both `model` and `level` clears any override.
 */
export function applyForkModelChoice(
	orchestrator: Orchestrator,
	model: Model<any> | undefined,
	level: ThinkingLevel | undefined,
	settingsManager?: SettingsManager,
): void {
	orchestrator.setForkModel(model);
	orchestrator.setForkThinkingLevel(level);
	if (!settingsManager) {
		return;
	}
	if (model) {
		// Atomic write: if `level` is undefined (e.g. the user picked a
		// non-reasoning model), that `undefined` must be persisted so an
		// earlier "high" setting from a reasoning model doesn't linger and
		// get rehydrated on next startup.
		settingsManager.setWorklogForkOverride(model.provider, model.id, level);
	} else {
		settingsManager.clearWorklogForkModel();
	}
}

function summarizeForkModel(
	orchestrator: Orchestrator,
	ctx: Pick<ExtensionCommandContext, "ui">,
): void {
	const model = orchestrator.getForkModel();
	const level = orchestrator.getForkThinkingLevel();
	if (model) {
		ctx.ui.notify(
			`Worklog fork override: ${model.provider}/${model.id}${level ? ` (${level})` : ""}.`,
			"info",
		);
	} else {
		ctx.ui.notify("Worklog fork override: unset (falls back to session model).", "info");
	}
}

async function runForkModelPicker(
	orchestrator: Orchestrator,
	ctx: ExtensionCommandContext,
	settingsManager: SettingsManager | undefined,
	prefilter: string | undefined,
): Promise<void> {
	ctx.modelRegistry.refresh();
	const availableModels = (await ctx.modelRegistry.getAvailable()) as Model<any>[];
	const options: string[] = [
		"‹unset› fall back to session model",
	];
	const normalizedPrefilter = prefilter?.trim().toLowerCase();
	const filtered = normalizedPrefilter
		? availableModels.filter(
				(model) =>
					model.id.toLowerCase().includes(normalizedPrefilter) ||
					model.provider.toLowerCase().includes(normalizedPrefilter),
			)
		: availableModels;
	for (const model of filtered) {
		options.push(`${model.provider}/${model.id}`);
	}
	if (options.length === 1) {
		ctx.ui.notify("No models match that filter.", "warning");
		return;
	}
	const choice = await ctx.ui.select("Worklog fork model", options);
	if (!choice) {
		return;
	}
	if (choice.startsWith("‹unset›")) {
		applyForkModelChoice(orchestrator, undefined, undefined, settingsManager);
		ctx.ui.notify("Worklog fork: cleared override (will use session model).", "info");
		return;
	}
	const match = resolveModelByReference(choice, availableModels);
	if (!match) {
		ctx.ui.notify(`Could not resolve '${choice}'.`, "error");
		return;
	}
	const levels = getThinkingLevels(match);
	let level: ThinkingLevel | undefined;
	if (levels.length > 0) {
		const picked = await ctx.ui.select(
			`Thinking level for ${match.id}`,
			[...levels, "‹default›"],
		);
		if (!picked) {
			return;
		}
		level = picked.startsWith("‹default›") ? resolveThinkingLevel(match, undefined) : (picked as ThinkingLevel);
	}
	applyForkModelChoice(orchestrator, match, level, settingsManager);
	ctx.ui.notify(
		`Worklog fork: ${match.provider}/${match.id}${level ? ` (${level})` : ""}.`,
		"info",
	);
}
