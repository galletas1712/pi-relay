import type { Model } from "@pi-relay/ai";
import { describe, expect, test } from "vitest";
import {
	AntigravitySource,
	EnvironmentSource,
	PromptAssembly,
	ProjectSource,
	type PromptContext,
	RoleSource,
	SkillsSource,
} from "../src/core/prompt/index.js";
import type { Skill } from "../src/core/skills.js";

const ROLE_PREAMBLE = "You are Claude Code, Anthropic's official CLI for Claude.";

function modelFor(provider: string): Model<"anthropic-messages"> {
	return {
		id: "test-model",
		name: "Test Model",
		api: "anthropic-messages",
		provider,
		baseUrl: "https://example.test",
		reasoning: false,
		input: ["text"],
		cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 },
		contextWindow: 128000,
		maxTokens: 4096,
	} as Model<"anthropic-messages">;
}

function assembleRole(
	options: Parameters<typeof RoleSource>[0] = {},
	ctxOverrides: Partial<PromptContext> = { model: modelFor("anthropic") },
): string {
	const assembly = new PromptAssembly([new RoleSource(options)]);
	return assembly.assemble(makeCtx(ctxOverrides)).text;
}

function makeCtx(overrides: Partial<PromptContext> = {}): PromptContext {
	return {
		sessionId: "test-session",
		cwd: "/tmp/test",
		now: new Date("2026-04-16T00:00:00Z"),
		toolNames: ["read"],
		...overrides,
	};
}

describe("RoleSource", () => {
	describe("empty tools", () => {
		test("shows (none) for empty tools list", () => {
			const prompt = assembleRole({ selectedTools: [] });
			expect(prompt).toContain("Available tools:\n(none)");
		});

		test("shows file paths guideline even with no tools", () => {
			const prompt = assembleRole({ selectedTools: [] });
			expect(prompt).toContain("Show file paths clearly");
		});
	});

	describe("default tools", () => {
		test("includes all default tools when snippets are provided", () => {
			const prompt = assembleRole({
				toolSnippets: {
					read: "Read file contents",
					bash: "Execute bash commands",
					edit: "Make surgical edits",
					write: "Create or overwrite files",
				},
			});

			expect(prompt).toContain("- read:");
			expect(prompt).toContain("- bash:");
			expect(prompt).toContain("- edit:");
			expect(prompt).toContain("- write:");
		});
	});

	describe("custom tool snippets", () => {
		test("includes custom tools in available tools section when promptSnippet is provided", () => {
			const prompt = assembleRole({
				selectedTools: ["read", "dynamic_tool"],
				toolSnippets: {
					dynamic_tool: "Run dynamic test behavior",
				},
			});

			expect(prompt).toContain("- dynamic_tool: Run dynamic test behavior");
		});

		test("omits custom tools from available tools section when promptSnippet is not provided", () => {
			const prompt = assembleRole({ selectedTools: ["read", "dynamic_tool"] });
			expect(prompt).not.toContain("dynamic_tool");
		});
	});

	describe("role preamble", () => {
		test("prepends the Claude Code role preamble to the default prompt on Anthropic", () => {
			const prompt = assembleRole({ selectedTools: ["read"] });
			expect(prompt.startsWith(`${ROLE_PREAMBLE}\n\n`)).toBe(true);
		});

		test("prepends the Claude Code role preamble to a customPrompt on Anthropic", () => {
			const prompt = assembleRole({ customPrompt: "Custom base prompt.", selectedTools: ["read"] });
			expect(prompt.startsWith(`${ROLE_PREAMBLE}\n\nCustom base prompt.`)).toBe(true);
		});

		test.each([
			["amazon-bedrock"],
			["google-antigravity"],
			["openrouter"],
			["vercel-ai-gateway"],
			["openai"],
			["openai-codex"],
		])("omits the Claude Code role preamble on %s", (provider) => {
			const prompt = assembleRole({ selectedTools: ["read"] }, { model: modelFor(provider) });
			expect(prompt).not.toContain(ROLE_PREAMBLE);
		});

		test("omits the Claude Code role preamble when the context has no model", () => {
			const prompt = assembleRole({ selectedTools: ["read"] }, {});
			expect(prompt).not.toContain(ROLE_PREAMBLE);
		});
	});

	describe("prompt guidelines", () => {
		test("appends promptGuidelines to default guidelines", () => {
			const prompt = assembleRole({
				selectedTools: ["read", "dynamic_tool"],
				promptGuidelines: ["Use dynamic_tool for project summaries."],
			});

			expect(prompt).toContain("- Use dynamic_tool for project summaries.");
		});

		test("deduplicates and trims promptGuidelines", () => {
			const prompt = assembleRole({
				selectedTools: ["read", "dynamic_tool"],
				promptGuidelines: [
					"Use dynamic_tool for summaries.",
					"  Use dynamic_tool for summaries.  ",
					"   ",
				],
			});

			expect(prompt.match(/- Use dynamic_tool for summaries\./g)).toHaveLength(1);
		});
	});
});

describe("PromptAssembly", () => {
	test("orders sections per SECTION_ORDER and joins with blank lines", () => {
		const assembly = new PromptAssembly([
			new RoleSource({ selectedTools: ["read"] }),
			new ProjectSource([{ path: "/proj/AGENTS.md", content: "Project rule A" }]),
			new EnvironmentSource(),
		]);
		const prompt = assembly.assemble(makeCtx({ model: modelFor("anthropic") })).text;

		const roleIdx = prompt.indexOf(ROLE_PREAMBLE);
		const projectIdx = prompt.indexOf("# Project Context");
		const envIdx = prompt.indexOf("Current date:");

		expect(roleIdx).toBeGreaterThanOrEqual(0);
		expect(projectIdx).toBeGreaterThan(roleIdx);
		expect(envIdx).toBeGreaterThan(projectIdx);
	});

	test("emits project context files with AGENTS.md content verbatim", () => {
		const assembly = new PromptAssembly([
			new ProjectSource([{ path: "/proj/AGENTS.md", content: "Project rule A" }]),
		]);
		const prompt = assembly.assemble(makeCtx()).text;
		expect(prompt).toContain("# Project Context");
		expect(prompt).toContain("## /proj/AGENTS.md");
		expect(prompt).toContain("Project rule A");
	});

	test("skips the skills section when the read tool is absent", () => {
		const skill: Skill = makeSkill("shell", "helps with shell");
		const assembly = new PromptAssembly([new SkillsSource([skill])]);

		const withRead = assembly.assemble(makeCtx({ toolNames: ["read"] })).text;
		expect(withRead).toContain("<available_skills>");
		expect(withRead).toContain("shell");

		const withoutRead = assembly.assemble(makeCtx({ toolNames: ["bash"] })).text;
		expect(withoutRead).toBe("");
	});

	test("register rejects duplicate source names", () => {
		const assembly = new PromptAssembly([new RoleSource()]);
		expect(() => assembly.register(new RoleSource())).toThrow(/already registered/);
	});

	test("unregister removes a source", () => {
		const assembly = new PromptAssembly([new RoleSource()]);
		assembly.unregister("coding-agent.role");
		expect(assembly.assemble(makeCtx()).text).toBe("");
	});

	test("blocks expose per-section text and retention tiers", () => {
		const assembly = new PromptAssembly([
			new RoleSource({ selectedTools: ["read"] }),
			new EnvironmentSource(),
		]);
		const { blocks } = assembly.assemble(makeCtx());
		// role is retention "long"; environment is retention "none". They land in
		// separate blocks because their tiers differ.
		expect(blocks).toHaveLength(2);
		expect(blocks[0].sections).toEqual(["role"]);
		expect(blocks[0].retention).toBe("long");
		expect(blocks[1].sections).toEqual(["environment"]);
		expect(blocks[1].retention).toBe("none");
	});

	test("coalesces consecutive sections of the same retention tier into one block", () => {
		const assembly = new PromptAssembly([
			new RoleSource({ selectedTools: ["read"] }),
			new ProjectSource([{ path: "/proj/AGENTS.md", content: "Project rule A" }]),
			new EnvironmentSource(),
		]);
		const { blocks } = assembly.assemble(makeCtx());
		// role (long) → project (short) → environment (none). Three distinct tiers,
		// three blocks.
		expect(blocks.map((b) => b.retention)).toEqual(["long", "short", "none"]);
		expect(blocks[0].sections).toEqual(["role"]);
		expect(blocks[1].sections).toEqual(["project"]);
		expect(blocks[2].sections).toEqual(["environment"]);
	});

	test("AntigravitySource injects handshake fragment only for google-antigravity", () => {
		const assembly = new PromptAssembly([
			new RoleSource({ selectedTools: ["read"] }),
			new AntigravitySource(),
		]);

		const antigravityPrompt = assembly.assemble(makeCtx({ model: modelFor("google-antigravity") })).text;
		expect(antigravityPrompt).toContain("You are Antigravity");
		expect(antigravityPrompt).toContain("[ignore]");
		expect(antigravityPrompt).toContain("[/ignore]");
		// Fragment lands inside the role section with priority -10, so it sits before the
		// role preamble / default body.
		const antigravityIdx = antigravityPrompt.indexOf("You are Antigravity");
		const pidx = antigravityPrompt.indexOf("expert coding assistant");
		expect(antigravityIdx).toBeGreaterThanOrEqual(0);
		expect(pidx).toBeGreaterThan(antigravityIdx);

		const anthropicPrompt = assembly.assemble(makeCtx({ model: modelFor("anthropic") })).text;
		expect(anthropicPrompt).not.toContain("You are Antigravity");

		const noModelPrompt = assembly.assemble(makeCtx()).text;
		expect(noModelPrompt).not.toContain("You are Antigravity");
	});

	test("environment block reflects the injected date and cwd", () => {
		const prompt = new PromptAssembly([new EnvironmentSource()])
			.assemble(makeCtx({ cwd: "C:\\foo\\bar", now: new Date("2030-01-05T12:00:00Z") }))
			.text;
		expect(prompt).toContain("Current date: 2030-01-05");
		expect(prompt).toContain("Current working directory: C:/foo/bar");
	});
});

function makeSkill(name: string, description: string): Skill {
	return {
		name,
		description,
		filePath: `/skills/${name}/SKILL.md`,
		baseDir: `/skills/${name}`,
		sourceInfo: {
			path: `/skills/${name}/SKILL.md`,
			source: "local",
			scope: "user",
			origin: "top-level",
		},
		disableModelInvocation: false,
	};
}
