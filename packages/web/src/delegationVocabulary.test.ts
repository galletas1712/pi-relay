import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { describe, expect, it } from "vitest";

const activeFrontendSources = [
	"agentApi.ts",
	"types.ts",
	"queryKeys.ts",
	"delegationBoard.ts",
	"runBoard.tsx",
	"inspector.tsx",
	"App.tsx",
	"panels.tsx",
	"domain.css",
] as const;

function source(path: string): string {
	return readFileSync(resolve(import.meta.dirname, path), "utf8");
}

describe("delegation vocabulary in active frontend sources", () => {
	it("does not call legacy websocket methods", () => {
		for (const path of activeFrontendSources) {
			expect(source(path), `${path} must not call legacy websocket methods`).not.toMatch(
				/stage\.(start_full|start_readonly_fanout|status|cancel|list|read_handoff_file)/,
			);
		}
	});

	it("does not expose legacy delegation field or type vocabulary", () => {
		for (const path of activeFrontendSources) {
			const text = source(path);
			expect(text, `${path} must use delegation_id`).not.toContain("stage_id");
			expect(text, `${path} must use delegationId`).not.toContain("stageId");
			expect(text, `${path} must use Delegation types`).not.toMatch(/\bStage[A-Z]/);
			expect(text, `${path} must use delegation lists`).not.toMatch(/\bstages\b/);
			expect(text, `${path} must use delegation CSS classes`).not.toContain("run-board-stage");
		}
	});
});
