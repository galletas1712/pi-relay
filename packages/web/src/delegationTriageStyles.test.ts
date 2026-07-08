import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { describe, expect, it } from "vitest";

const styles = readFileSync(resolve(import.meta.dirname, "styles.css"), "utf8");

describe("minimal agent outline geometry", () => {
	it("uses flat rows without card borders, pills, section chrome, or ambient pulse", () => {
		expect(styles).toMatch(/\.run-board-outline\s*\{[\s\S]*?flex-direction:\s*column;/);
		expect(styles).toMatch(/\.run-board-delegation\s*\{[\s\S]*?border-top:/);
		expect(styles).not.toMatch(/\.run-board-delegation\s*\{[^}]*border:\s*1px/);
		expect(styles).not.toMatch(/\.run-board-delegation\s*\{[^}]*border-radius:/);
		expect(styles).not.toMatch(/\.run-board-delegation\s*\{[^}]*box-shadow:/);
		expect(styles).not.toMatch(/\.run-board-status-icon\.running\s*\{[^}]*animation:/);
		for (const removed of [
			"run-board-group > h2",
			"run-board-delegation-kind",
			"run-board-delegation-meta",
			"run-board-progress",
			"run-board-subagent-status",
			"run-board-subagent-outcome",
			"chip-button",
		]) {
			expect(styles).not.toContain(`.${removed}`);
		}
	});

	it("keeps narrow geometry one-line and direct actions at 44px for coarse pointers", () => {
		expect(styles).toMatch(/@container \(max-width: 380px\)[\s\S]*?\.run-board-delegation-head[\s\S]*?grid-template-columns:\s*18px minmax\(0, 1fr\) auto;/);
		expect(styles).toMatch(/@media \(pointer: coarse\)[\s\S]*?\.run-board-cancel,[\s\S]*?\.run-board-subagent-button,[\s\S]*?min-height:\s*44px;/);
		expect(styles).toMatch(/@media \(pointer: coarse\)[\s\S]*?\.run-board-cancel\s*\{[\s\S]*?width:\s*44px;/);
		expect(styles).toMatch(/@media \(pointer: coarse\)[\s\S]*?\.cancel-delegation-dialog \.plain-close-button[\s\S]*?min-width:\s*44px;/);
	});

	it("styles the role as quiet secondary text beneath the agent name", () => {
		expect(styles).toMatch(/\.run-board-subagent-copy\s*\{[\s\S]*?flex-direction:\s*column;/);
		expect(styles).toMatch(/\.run-board-subagent-name\s*\{[\s\S]*?font-weight:\s*500;/);
		expect(styles).toMatch(/\.run-board-subagent-role\s*\{[\s\S]*?color:\s*var\(--muted-foreground\);[\s\S]*?font-size:\s*var\(--text-xs\);/);
	});
});
