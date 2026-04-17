import { describe, expect, test } from "vitest";
import {
	EnvironmentSource,
	PromptAssembly,
	ProjectSource,
	type PromptContext,
	RoleSource,
	SkillsSource,
} from "../src/core/prompt/index.js";
import type { Skill } from "../src/core/skills.js";

const ROLE_PREAMBLE = "You are Claude Code, Anthropic's official CLI for Claude.";

function assembleRole(options: Parameters<typeof RoleSource>[0] = {}): string {
	const assembly = new PromptAssembly([new RoleSource(options)]);
	return assembly.assemble(makeCtx()).text;
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
		test("prepends the Claude Code role preamble to the default prompt", () => {
			const prompt = assembleRole({ selectedTools: ["read"] });
			expect(prompt.startsWith(`${ROLE_PREAMBLE}\n\n`)).toBe(true);
		});

		test("prepends the Claude Code role preamble to a customPrompt", () => {
			const prompt = assembleRole({ customPrompt: "Custom base prompt.", selectedTools: ["read"] });
			expect(prompt.startsWith(`${ROLE_PREAMBLE}\n\nCustom base prompt.`)).toBe(true);
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
		const prompt = assembly.assemble(makeCtx()).text;

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

	test("blocks expose per-section text and cache hints", () => {
		const assembly = new PromptAssembly([
			new RoleSource({ selectedTools: ["read"] }),
			new EnvironmentSource(),
		]);
		const { blocks } = assembly.assemble(makeCtx());
		const sections = blocks.map((block) => block.section);
		expect(sections).toEqual(["role", "environment"]);
		expect(blocks.every((block) => block.cacheable)).toBe(true);
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
