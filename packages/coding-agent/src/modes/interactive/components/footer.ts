import { type Component, truncateToWidth, visibleWidth } from "@pi-relay/tui";
import type { AgentSession } from "../../../core/agent-session.js";
import {
	formatCompactTokens,
	formatSelfTreeCost,
	formatTurnCacheStats,
	isCacheStatsEnabled,
} from "../../../core/cache-telemetry.js";
import type { ReadonlyFooterDataProvider } from "../../../core/footer-data-provider.js";
import { theme } from "../theme/theme.js";

/**
 * Sanitize text for display in a single-line status.
 * Removes newlines, tabs, carriage returns, and other control characters.
 */
function sanitizeStatusText(text: string): string {
	// Replace newlines, tabs, carriage returns with space, then collapse multiple spaces
	return text
		.replace(/[\r\n\t]/g, " ")
		.replace(/ +/g, " ")
		.trim();
}

/**
 * Format token counts (similar to web-ui).
 *
 * Thin wrapper around `formatCompactTokens` so existing callers and future
 * footer components share one abbreviation rule set.
 */
function formatTokens(count: number): string {
	return formatCompactTokens(count);
}

/**
 * Footer component that shows pwd, token stats, and context usage.
 * Computes token/context stats from session, gets git branch and extension statuses from provider.
 */
export class FooterComponent implements Component {
	private autoCompactEnabled = true;

	constructor(
		private session: AgentSession,
		private footerData: ReadonlyFooterDataProvider,
	) {}

	setSession(session: AgentSession): void {
		this.session = session;
	}

	setAutoCompactEnabled(enabled: boolean): void {
		this.autoCompactEnabled = enabled;
	}

	/**
	 * No-op: git branch caching now handled by provider.
	 * Kept for compatibility with existing call sites in interactive-mode.
	 */
	invalidate(): void {
		// No-op: git branch is cached/invalidated by provider
	}

	/**
	 * Clean up resources.
	 * Git watcher cleanup now handled by provider.
	 */
	dispose(): void {
		// Git watcher cleanup handled by provider
	}

	render(width: number): string[] {
		const state = this.session.state;

		// Calculate cumulative usage from ALL session entries (not just post-compaction messages)
		let totalInput = 0;
		let totalOutput = 0;
		let totalCacheRead = 0;
		let totalCacheWrite = 0;
		let totalCost = 0;
		// Per-turn cache telemetry (PR-4): surface the LAST assistant turn's cache
		// read/write counts so developers can attribute hits to the turn that
		// produced them. Gated on PI_SHOW_CACHE_STATS=1 at render time.
		let lastTurnCacheRead = 0;
		let lastTurnCacheWrite = 0;

		for (const entry of this.session.sessionManager.getEntries()) {
			if (entry.type === "message" && entry.message.role === "assistant") {
				totalInput += entry.message.usage.input;
				totalOutput += entry.message.usage.output;
				totalCacheRead += entry.message.usage.cacheRead;
				totalCacheWrite += entry.message.usage.cacheWrite;
				totalCost += entry.message.usage.cost.total;
				lastTurnCacheRead = entry.message.usage.cacheRead;
				lastTurnCacheWrite = entry.message.usage.cacheWrite;
			}
		}

		// Calculate context usage from session (handles compaction correctly).
		// After compaction, tokens are unknown until the next LLM response.
		const contextUsage = this.session.getContextUsage();
		const contextWindow = contextUsage?.contextWindow ?? state.model?.contextWindow ?? 0;
		const contextPercentValue = contextUsage?.percent ?? 0;
		const contextPercent = contextUsage?.percent !== null ? contextPercentValue.toFixed(1) : "?";

		// Replace home directory with ~
		let pwd = this.session.sessionManager.getCwd();
		const home = process.env.HOME || process.env.USERPROFILE;
		if (home && pwd.startsWith(home)) {
			pwd = `~${pwd.slice(home.length)}`;
		}

		// Add git branch if available
		const branch = this.footerData.getGitBranch();
		if (branch) {
			pwd = `${pwd} (${branch})`;
		}

		// Add session name if set
		const sessionName = this.session.sessionManager.getSessionName();
		if (sessionName) {
			pwd = `${pwd} • ${sessionName}`;
		}

		// Subtree usage (from orchestrator-style extensions). Undefined for
		// single-agent sessions. When present, the tokens above reflect the
		// session's local message history — which should match
		// subtree.self.tokens.* computed by the orchestrator from the same
		// messages. We still pull the tree-side numbers through the same code
		// path so aggregation logic is localized to the orchestrator.
		const subtree = this.session.getSubtreeUsage();
		const selfInput = subtree?.self.tokens.input ?? totalInput;
		const selfOutput = subtree?.self.tokens.output ?? totalOutput;
		const selfCacheRead = subtree?.self.tokens.cacheRead ?? totalCacheRead;
		const selfCacheWrite = subtree?.self.tokens.cacheWrite ?? totalCacheWrite;
		const selfCost = subtree?.self.cost ?? totalCost;
		const showTree = subtree?.hasDescendants === true;

		// Build stats line
		const statsParts: string[] = [];
		if (selfInput) statsParts.push(`↑${formatTokens(selfInput)}`);
		if (selfOutput) statsParts.push(`↓${formatTokens(selfOutput)}`);
		if (selfCacheRead) statsParts.push(`R${formatTokens(selfCacheRead)}`);
		if (selfCacheWrite) statsParts.push(`W${formatTokens(selfCacheWrite)}`);

		// When the attached agent has descendants, surface the tree token deltas
		// inline so the user can see subagent contribution at a glance. Inline
		// column keeps the footer layout single-line while staying readable.
		if (showTree && subtree) {
			const treeTokenParts: string[] = [];
			if (subtree.tree.tokens.input !== selfInput) {
				treeTokenParts.push(`↑${formatTokens(subtree.tree.tokens.input)}`);
			}
			if (subtree.tree.tokens.output !== selfOutput) {
				treeTokenParts.push(`↓${formatTokens(subtree.tree.tokens.output)}`);
			}
			if (subtree.tree.tokens.cacheRead !== selfCacheRead) {
				treeTokenParts.push(`R${formatTokens(subtree.tree.tokens.cacheRead)}`);
			}
			if (subtree.tree.tokens.cacheWrite !== selfCacheWrite) {
				treeTokenParts.push(`W${formatTokens(subtree.tree.tokens.cacheWrite)}`);
			}
			if (treeTokenParts.length > 0) {
				statsParts.push(`tree[${treeTokenParts.join(" ")}]`);
			}
		}

		// Per-turn cache delta (dev-only, gated on PI_SHOW_CACHE_STATS=1).
		if (isCacheStatsEnabled()) {
			const turnStats = formatTurnCacheStats({ cacheRead: lastTurnCacheRead, cacheWrite: lastTurnCacheWrite });
			if (turnStats) statsParts.push(turnStats);
		}

		// Show cost with "(sub)" indicator only for subscription-backed auth.
		// When a subtree is present and tree > self, expand cost to `self $X · tree $Y`.
		const usingSubscription = state.model ? this.session.modelRegistry.isUsingSubscriptionAuth(state.model) : false;
		const subscriptionSuffix = usingSubscription ? " (sub)" : "";
		if (showTree && subtree) {
			const costStr = formatSelfTreeCost({
				selfCost,
				treeCost: subtree.tree.cost,
				suffix: subscriptionSuffix,
				alwaysShowTree: true,
			});
			if (costStr) statsParts.push(costStr);
		} else if (selfCost || usingSubscription) {
			const costStr = `$${selfCost.toFixed(3)}${subscriptionSuffix}`;
			statsParts.push(costStr);
		}

		// Colorize context percentage based on usage
		let contextPercentStr: string;
		const autoIndicator = this.autoCompactEnabled ? " (auto)" : "";
		const contextPercentDisplay =
			contextPercent === "?"
				? `?/${formatTokens(contextWindow)}${autoIndicator}`
				: `${contextPercent}%/${formatTokens(contextWindow)}${autoIndicator}`;
		if (contextPercentValue > 90) {
			contextPercentStr = theme.fg("error", contextPercentDisplay);
		} else if (contextPercentValue > 70) {
			contextPercentStr = theme.fg("warning", contextPercentDisplay);
		} else {
			contextPercentStr = contextPercentDisplay;
		}
		statsParts.push(contextPercentStr);

		let statsLeft = statsParts.join(" ");

		// Add model name on the right side, plus thinking level if model supports it
		const modelName = state.model?.id || "no-model";

		let statsLeftWidth = visibleWidth(statsLeft);

		// If statsLeft is too wide, truncate it
		if (statsLeftWidth > width) {
			statsLeft = truncateToWidth(statsLeft, width, "...");
			statsLeftWidth = visibleWidth(statsLeft);
		}

		// Calculate available space for padding (minimum 2 spaces between stats and model)
		const minPadding = 2;

		// Add thinking level indicator if model supports reasoning
		let rightSideWithoutProvider = modelName;
		if (state.model?.reasoning) {
			rightSideWithoutProvider = `${modelName} • ${state.thinkingLevel}`;
		}

		// Prepend the provider in parentheses if there are multiple providers and there's enough room
		let rightSide = rightSideWithoutProvider;
		if (this.footerData.getAvailableProviderCount() > 1 && state.model) {
			rightSide = `(${state.model!.provider}) ${rightSideWithoutProvider}`;
			if (statsLeftWidth + minPadding + visibleWidth(rightSide) > width) {
				// Too wide, fall back
				rightSide = rightSideWithoutProvider;
			}
		}

		const rightSideWidth = visibleWidth(rightSide);
		const totalNeeded = statsLeftWidth + minPadding + rightSideWidth;

		let statsLine: string;
		if (totalNeeded <= width) {
			// Both fit - add padding to right-align model
			const padding = " ".repeat(width - statsLeftWidth - rightSideWidth);
			statsLine = statsLeft + padding + rightSide;
		} else {
			// Need to truncate right side
			const availableForRight = width - statsLeftWidth - minPadding;
			if (availableForRight > 0) {
				const truncatedRight = truncateToWidth(rightSide, availableForRight, "");
				const truncatedRightWidth = visibleWidth(truncatedRight);
				const padding = " ".repeat(Math.max(0, width - statsLeftWidth - truncatedRightWidth));
				statsLine = statsLeft + padding + truncatedRight;
			} else {
				// Not enough space for right side at all
				statsLine = statsLeft;
			}
		}

		// Apply dim to each part separately. statsLeft may contain color codes (for context %)
		// that end with a reset, which would clear an outer dim wrapper. So we dim the parts
		// before and after the colored section independently.
		const dimStatsLeft = theme.fg("dim", statsLeft);
		const remainder = statsLine.slice(statsLeft.length); // padding + rightSide
		const dimRemainder = theme.fg("dim", remainder);

		const pwdLine = truncateToWidth(theme.fg("dim", pwd), width, theme.fg("dim", "..."));
		const lines = [pwdLine, dimStatsLeft + dimRemainder];

		// Add extension statuses on a single line, sorted by key alphabetically
		const extensionStatuses = this.footerData.getExtensionStatuses();
		if (extensionStatuses.size > 0) {
			const sortedStatuses = Array.from(extensionStatuses.entries())
				.sort(([a], [b]) => a.localeCompare(b))
				.map(([, text]) => sanitizeStatusText(text));
			const statusLine = sortedStatuses.join(" ");
			// Truncate to terminal width with dim ellipsis for consistency with footer style
			lines.push(truncateToWidth(statusLine, width, theme.fg("dim", "...")));
		}

		return lines;
	}
}
