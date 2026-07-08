import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { describe, expect, it } from "vitest";

const styles = readFileSync(resolve(import.meta.dirname, "styles.css"), "utf8");

describe("agent navigator narrow and coarse-pointer geometry", () => {
	it("uses inspector inline-size containment and wraps header actions below 380px", () => {
		expect(styles).toMatch(/\.inspector\s*\{[\s\S]*?container-type:\s*inline-size;/);
		expect(styles).toMatch(/@container \(max-width: 380px\)[\s\S]*?\.run-board-delegation-head[\s\S]*?grid-template-columns: 18px minmax\(0, 1fr\);/);
		expect(styles).toMatch(/@container \(max-width: 380px\)[\s\S]*?\.run-board-delegation-controls,[\s\S]*?\.run-board-blocked-reason[\s\S]*?grid-column: 2;/);
	});

	it("gives direct agent actions 44px coarse-pointer targets", () => {
		expect(styles).toMatch(/@media \(pointer: coarse\)[\s\S]*?\.run-board-delegation-controls \.chip-button,[\s\S]*?\.run-board-subagent-button[\s\S]*?min-height: 44px;/);
		expect(styles).toMatch(/@media \(pointer: coarse\)[\s\S]*?\.cancel-delegation-dialog \.primary-button,[\s\S]*?\.cancel-delegation-dialog \.secondary-button[\s\S]*?min-height: 44px;/);
		expect(styles).toMatch(/@media \(pointer: coarse\)[\s\S]*?\.cancel-delegation-dialog \.plain-close-button[\s\S]*?min-width: 44px;/);
	});

	it("keeps essential agent copy at 12px or larger without pulsing every running row", () => {
		for (const selector of [
			"run-board-delegation-kind",
			"run-board-delegation-meta",
			"chip-button",
			"run-board-subagent-status",
			"run-board-subagent-outcome",
		]) {
			expect(styles).toMatch(
				new RegExp(`\\.${selector.replaceAll("-", "\\-")}[\\s\\S]*?font-size:\\s*var\\(--text-xs\\);`),
			);
		}
		expect(styles).not.toMatch(/\.run-board-status-icon\.running\s*\{[^}]*animation:/);
	});
});
