import { readFile } from "node:fs/promises";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import { CompactHistoryPickerDialog } from "../src/historyPickerCompact.tsx";
import type { TranscriptTreeNode } from "../src/types.ts";

const baseTime = Date.UTC(2026, 0, 1, 12, 0, 0);

function node(
	id: string,
	parentId: string | null,
	sequence: number,
	itemType: TranscriptTreeNode["item_type"],
	displayHint: string
): TranscriptTreeNode {
	return {
		id,
		parent_id: parentId,
		timestamp_ms: baseTime + sequence,
		sequence,
		item_type: itemType,
		turn_id: itemType === "turn_finished" ? sequence : null,
		outcome: itemType === "turn_finished" ? "Graceful" : null,
		can_switch_to: itemType === "turn_finished",
		edit_target_leaf_id: null,
		display_hint: displayHint
	};
}

function branchFixture(): TranscriptTreeNode[] {
	return [
		node("root", null, 1, "turn_finished", "root turn"),
		node("active", "root", 2, "turn_finished", "active branch"),
		node("alternate", "root", 3, "user_message", "alternate branch"),
		node("alternate-child", "alternate", 4, "turn_finished", "hidden alternate turn")
	];
}

function cssRule(source: string, selector: string): string {
	const escapedSelector = selector.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
	return source.match(new RegExp(`${escapedSelector}\\s*\\{([^}]*)\\}`))?.[1] ?? "";
}

function pixelDeclaration(rule: string, property: string): number {
	const value = rule.match(new RegExp(`${property}:\\s*(\\d+)px`))?.[1];
	return value ? Number(value) : 0;
}

describe("compact history branch disclosure", () => {
	it("renders collapsed branch disclosure separately from its switch target with contextual state", () => {
		const markup = renderToStaticMarkup(
			<CompactHistoryPickerDialog
				nodes={branchFixture()}
				activeLeafId="active"
				onClose={() => undefined}
				onSwitch={() => undefined}
			/>
		);
		const collapsedBranch = markup.match(
			/<div class="history-tree-item [^"]*\bcollapsed\b[^"]*"[^>]*>([\s\S]*?)<\/div>/
		)?.[1];
		const buttons = [...(collapsedBranch?.matchAll(/<button\b[^>]*>/g) ?? [])].map((match) => match[0]);

		expect(buttons).toHaveLength(2);
		expect(buttons[0]).toContain('class="branch-toggle"');
		expect(buttons[0]).toContain(
			'aria-label="Expand branch for User message: alternate branch, 1 hidden descendant"'
		);
		expect(buttons[0]).toContain('aria-expanded="false"');
		expect(buttons[1]).toContain('class="history-option tree-row"');
		expect(buttons[1]).toContain('role="treeitem"');
		expect(buttons[1]).toContain('aria-selected="false"');
		expect(collapsedBranch).toMatch(
			/<button class="branch-toggle"[\s\S]*?<\/button><button class="history-option tree-row"/
		);
		expect(collapsedBranch).toContain("alternate branch");
		expect(markup).not.toContain("hidden alternate turn");
	});

	it("keeps the compact disclosure visible with a coarse-pointer-sized target", async () => {
		const styles = await readFile(new URL("../src/styles.css", import.meta.url), "utf8");
		const compactStart = styles.indexOf("@media (max-width: 760px)");
		const compactEnd = styles.indexOf("@media (max-width: 430px)", compactStart);
		const compactStyles = styles.slice(compactStart, compactEnd);
		const toggleRule = cssRule(compactStyles, ".branch-toggle");

		expect(compactStart).toBeGreaterThanOrEqual(0);
		expect(compactEnd).toBeGreaterThan(compactStart);
		expect(toggleRule).toContain("display: inline-flex");
		expect(toggleRule).not.toContain("display: none");
		expect(pixelDeclaration(toggleRule, "width")).toBeGreaterThanOrEqual(44);
		expect(pixelDeclaration(toggleRule, "height")).toBeGreaterThanOrEqual(44);
	});
});
