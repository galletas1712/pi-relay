import { mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterEach, describe, expect, it, vi } from "vitest";
import { createRelayBaseToolDefinitionsFactory, RELAY_BASE_TOOL_NAMES } from "../../src/tools/base-tools.js";

type ToolResult = {
	content: Array<{ type: string; text?: string }>;
	details?: { diff?: string; changedFiles?: string[] };
};

const tempDirs: string[] = [];

afterEach(async () => {
	await Promise.all(
		tempDirs.splice(0).map(async (dir) => {
			await rm(dir, { recursive: true, force: true });
		}),
	);
});

async function makeTempDir(): Promise<string> {
	const dir = await mkdtemp(join(tmpdir(), "pi-relay-tools-"));
	tempDirs.push(dir);
	return dir;
}

function getText(result: ToolResult): string {
	return result.content
		.filter((content) => content.type === "text")
		.map((content) => content.text ?? "")
		.join("\n");
}

function getTool(definitions: ReturnType<ReturnType<typeof createRelayBaseToolDefinitionsFactory>>, name: string) {
	const definition = definitions.find((tool) => tool.name === name);
	if (!definition) {
		throw new Error(`Missing tool ${name}`);
	}
	return definition;
}

describe("relay base tools", () => {
	it("rebuilds the bundle with fresh settings and a shared tracker", async () => {
		const cwd = await makeTempDir();
		const filePath = join(cwd, "notes.txt");
		await writeFile(filePath, "alpha\nbeta\n", "utf-8");

		const settingsManager = {
			getImageAutoResize: vi.fn().mockReturnValueOnce(true).mockReturnValueOnce(false),
			getShellCommandPrefix: vi
				.fn()
				.mockReturnValueOnce("export RELAY_PREFIX=first")
				.mockReturnValueOnce("export RELAY_PREFIX=second"),
		};

		const factory = createRelayBaseToolDefinitionsFactory(cwd, settingsManager as never);
		expect(RELAY_BASE_TOOL_NAMES).toEqual(["read", "bash", "edit", "apply_patch", "write"]);

		const firstBundle = factory();
		const readTool = getTool(firstBundle, "read");
		const firstRead = await readTool.execute("read-1", { path: "notes.txt" }, undefined, undefined, undefined as never);
		expect(getText(firstRead as ToolResult)).toContain("alpha");

		const secondBundle = factory();
		const editTool = getTool(secondBundle, "edit");
		const editResult = (await editTool.execute(
			"edit-1",
			{ path: "notes.txt", edits: [{ oldText: "beta", newText: "gamma" }] },
			undefined,
			undefined,
			undefined as never,
		)) as ToolResult;
		expect(getText(editResult)).toContain("Successfully replaced 1 block");
		expect(await readFile(filePath, "utf-8")).toContain("gamma");

		const bashTool = getTool(secondBundle, "bash");
		const bashResult = (await bashTool.execute(
			"bash-1",
			{ command: 'printf "%s" "$RELAY_PREFIX"' },
			undefined,
			undefined,
			undefined as never,
		)) as ToolResult;
		expect(getText(bashResult)).toBe("second");

		expect(settingsManager.getImageAutoResize).toHaveBeenCalledTimes(2);
		expect(settingsManager.getShellCommandPrefix).toHaveBeenCalledTimes(2);
	});

	it("returns an unchanged stub after a repeated full read", async () => {
		const cwd = await makeTempDir();
		await writeFile(join(cwd, "same.txt"), "one\ntwo\n", "utf-8");
		const settingsManager = {
			getImageAutoResize: vi.fn(() => true),
			getShellCommandPrefix: vi.fn(() => undefined),
		};
		const factory = createRelayBaseToolDefinitionsFactory(cwd, settingsManager as never);
		const readTool = getTool(factory(), "read");

		const firstRead = (await readTool.execute(
			"read-1",
			{ path: "same.txt" },
			undefined,
			undefined,
			undefined as never,
		)) as ToolResult;
		const secondRead = (await readTool.execute(
			"read-2",
			{ path: "same.txt" },
			undefined,
			undefined,
			undefined as never,
		)) as ToolResult;

		expect(getText(firstRead)).toContain("one");
		expect(getText(secondRead)).toBe("[File unchanged since the last full read: same.txt]");
	});

	it("enforces read-before-mutate and stale-read checks across edit, write, and apply_patch", async () => {
		const cwd = await makeTempDir();
		const filePath = join(cwd, "tracked.txt");
		await writeFile(filePath, "hello\nworld\n", "utf-8");
		const settingsManager = {
			getImageAutoResize: vi.fn(() => true),
			getShellCommandPrefix: vi.fn(() => undefined),
		};
		const factory = createRelayBaseToolDefinitionsFactory(cwd, settingsManager as never);
		const definitions = factory();

		const editTool = getTool(definitions, "edit");
		await expect(
			editTool.execute(
				"edit-1",
				{ path: "tracked.txt", edits: [{ oldText: "world", newText: "relay" }] },
				undefined,
				undefined,
				undefined as never,
			),
		).rejects.toThrow("Read tracked.txt with read before using edit.");

		const readTool = getTool(definitions, "read");
		await readTool.execute(
			"read-1",
			{ path: "tracked.txt", limit: 1 },
			undefined,
			undefined,
			undefined as never,
		);

		const writeTool = getTool(definitions, "write");
		await expect(
			writeTool.execute(
				"write-1",
				{ path: "tracked.txt", content: "full rewrite\n" },
				undefined,
				undefined,
				undefined as never,
			),
		).rejects.toThrow("Read the full current contents of tracked.txt with read before using write.");

		await readTool.execute("read-2", { path: "tracked.txt" }, undefined, undefined, undefined as never);
		await writeFile(filePath, "hello\nchanged\n", "utf-8");

		const applyPatchTool = getTool(definitions, "apply_patch");
		await expect(
			applyPatchTool.execute(
				"patch-1",
				{
					patch: [
						"*** Begin Patch",
						"*** Update File: tracked.txt",
						"@@",
						" hello",
						"-changed",
						"+patched",
						"*** End Patch",
					].join("\n"),
				},
				undefined,
				undefined,
				undefined as never,
			),
		).rejects.toThrow("Read tracked.txt again before using apply_patch because the file changed since the last read.");
	});

	it("supports successful multi-file apply_patch and full-file write flows after valid reads", async () => {
		const cwd = await makeTempDir();
		await writeFile(join(cwd, "alpha.txt"), "alpha\none\n", "utf-8");
		await writeFile(join(cwd, "beta.txt"), "beta\ntwo\n", "utf-8");
		const settingsManager = {
			getImageAutoResize: vi.fn(() => true),
			getShellCommandPrefix: vi.fn(() => undefined),
		};
		const definitions = createRelayBaseToolDefinitionsFactory(cwd, settingsManager as never)();
		const readTool = getTool(definitions, "read");
		const applyPatchTool = getTool(definitions, "apply_patch");
		const writeTool = getTool(definitions, "write");

		await readTool.execute("read-a", { path: "alpha.txt" }, undefined, undefined, undefined as never);
		await readTool.execute("read-b", { path: "beta.txt" }, undefined, undefined, undefined as never);

		const patchResult = (await applyPatchTool.execute(
			"patch-1",
			{
				patch: [
					"*** Begin Patch",
					"*** Update File: alpha.txt",
					"@@",
					" alpha",
					"-one",
					"+uno",
					"*** Update File: beta.txt",
					"@@",
					" beta",
					"-two",
					"+dos",
					"*** End Patch",
				].join("\n"),
			},
			undefined,
			undefined,
			undefined as never,
		)) as ToolResult;

		expect(getText(patchResult)).toContain("Applied patch: 2 updated.");
		expect(patchResult.details?.changedFiles).toEqual(["alpha.txt", "beta.txt"]);
		expect(await readFile(join(cwd, "alpha.txt"), "utf-8")).toBe("alpha\nuno\n");
		expect(await readFile(join(cwd, "beta.txt"), "utf-8")).toBe("beta\ndos\n");

		await readTool.execute("read-c", { path: "alpha.txt" }, undefined, undefined, undefined as never);
		const writeResult = (await writeTool.execute(
			"write-1",
			{ path: "alpha.txt", content: "alpha\nrewritten\n" },
			undefined,
			undefined,
			undefined as never,
		)) as ToolResult;

		expect(getText(writeResult)).toContain("Successfully wrote");
		expect(await readFile(join(cwd, "alpha.txt"), "utf-8")).toBe("alpha\nrewritten\n");
	});
});
