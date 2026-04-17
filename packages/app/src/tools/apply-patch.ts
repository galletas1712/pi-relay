import { constants } from "node:fs";
import { access, mkdir, readFile, rm, writeFile } from "node:fs/promises";
import { dirname, resolve } from "node:path";
import type { ToolDefinition } from "@pi-relay/coding-agent";
import { type Static, Type } from "@sinclair/typebox";
import {
	applyEditsToNormalizedContent,
	detectLineEnding,
	generateDiffString,
	normalizeToLF,
	restoreLineEndings,
	stripBom,
	type Edit,
} from "./edit-diff.js";
import { type FileAccessTracker, fingerprintFileContent } from "./file-access-tracker.js";
import { withFileMutationQueue } from "./file-mutation-queue.js";
import { resolveToCwd } from "./path-utils.js";

const applyPatchSchema = Type.Object(
	{
		patch: Type.String({
			description:
				"Patch text in apply_patch format. Use *** Begin Patch / *** End Patch with Add File, Delete File, or Update File hunks.",
		}),
	},
	{ additionalProperties: false },
);

export type ApplyPatchToolInput = Static<typeof applyPatchSchema>;

export interface ApplyPatchToolDetails {
	diff: string;
	changedFiles: string[];
}

export interface ApplyPatchToolOptions {
	tracker?: FileAccessTracker;
}

type PatchLine = {
	type: "context" | "add" | "remove";
	text: string;
};

type PatchBlock = {
	lines: PatchLine[];
	endsAtFileEnd: boolean;
};

type ParsedPatchOperation =
	| { type: "add"; path: string; content: string }
	| { type: "delete"; path: string }
	| { type: "update"; path: string; moveTo?: string; edits: Edit[] };

type ParsedPatch = {
	operations: ParsedPatchOperation[];
	touchedPaths: string[];
};

function isOperationHeader(line: string): boolean {
	return (
		line.startsWith("*** Add File: ") ||
		line.startsWith("*** Delete File: ") ||
		line.startsWith("*** Update File: ") ||
		line === "*** End Patch"
	);
}

function parseOperationPath(line: string, prefix: string): string {
	const path = line.slice(prefix.length).trim();
	if (!path) {
		throw new Error(`Missing path in patch line: ${line}`);
	}
	return path;
}

function renderPatchText(lines: PatchLine[], types: Set<PatchLine["type"]>, endsAtFileEnd: boolean): string {
	const selected = lines.filter((line) => types.has(line.type)).map((line) => line.text);
	if (selected.length === 0) {
		return "";
	}
	const text = selected.join("\n");
	return endsAtFileEnd ? text : `${text}\n`;
}

function parsePatchBlock(block: PatchBlock, path: string): Edit | null {
	const hasChange = block.lines.some((line) => line.type !== "context");
	if (!hasChange) {
		return null;
	}

	const oldText = renderPatchText(block.lines, new Set(["context", "remove"]), block.endsAtFileEnd);
	const newText = renderPatchText(block.lines, new Set(["context", "add"]), block.endsAtFileEnd);
	if (oldText.length === 0) {
		throw new Error(
			`Patch hunk for ${path} does not contain enough context to anchor the change. Use Update File with nearby context lines or Add File for brand-new files.`,
		);
	}
	return { oldText, newText };
}

function parseUpdatePatch(path: string, moveTo: string | undefined, bodyLines: string[]): ParsedPatchOperation {
	const blocks: PatchBlock[] = [];
	let lines: PatchLine[] = [];
	let endsAtFileEnd = false;

	const flush = () => {
		if (lines.length === 0) {
			endsAtFileEnd = false;
			return;
		}
		blocks.push({ lines, endsAtFileEnd });
		lines = [];
		endsAtFileEnd = false;
	};

	for (const bodyLine of bodyLines) {
		if (bodyLine === "*** End of File") {
			endsAtFileEnd = true;
			continue;
		}
		if (bodyLine === "@@" || bodyLine.startsWith("@@ ")) {
			flush();
			continue;
		}

		const prefix = bodyLine[0];
		if (prefix !== " " && prefix !== "+" && prefix !== "-") {
			throw new Error(`Invalid patch line for ${path}: ${bodyLine}`);
		}
		lines.push({
			type: prefix === " " ? "context" : prefix === "+" ? "add" : "remove",
			text: bodyLine.slice(1),
		});
	}
	flush();

	const edits = blocks
		.map((block) => parsePatchBlock(block, path))
		.filter((edit): edit is Edit => edit !== null);
	if (edits.length === 0 && !moveTo) {
		throw new Error(`Update File patch for ${path} did not contain any changes.`);
	}
	return { type: "update", path, moveTo, edits };
}

function parseApplyPatch(patch: string): ParsedPatch {
	const lines = normalizeToLF(patch).split("\n");
	if (lines[lines.length - 1] === "") {
		lines.pop();
	}
	if (lines[0] !== "*** Begin Patch") {
		throw new Error("Patch must start with *** Begin Patch");
	}
	if (lines[lines.length - 1] !== "*** End Patch") {
		throw new Error("Patch must end with *** End Patch");
	}

	const operations: ParsedPatchOperation[] = [];
	for (let index = 1; index < lines.length - 1; ) {
		const line = lines[index]!;
		if (line.startsWith("*** Add File: ")) {
			const path = parseOperationPath(line, "*** Add File: ");
			index++;
			const contentLines: string[] = [];
			while (index < lines.length - 1 && !isOperationHeader(lines[index]!)) {
				const bodyLine = lines[index]!;
				if (!bodyLine.startsWith("+")) {
					throw new Error(`Add File patch for ${path} must only contain '+' lines. Found: ${bodyLine}`);
				}
				contentLines.push(bodyLine.slice(1));
				index++;
			}
			if (contentLines.length === 0) {
				throw new Error(`Add File patch for ${path} must contain at least one line.`);
			}
			operations.push({ type: "add", path, content: `${contentLines.join("\n")}\n` });
			continue;
		}

		if (line.startsWith("*** Delete File: ")) {
			operations.push({ type: "delete", path: parseOperationPath(line, "*** Delete File: ") });
			index++;
			continue;
		}

		if (line.startsWith("*** Update File: ")) {
			const path = parseOperationPath(line, "*** Update File: ");
			index++;
			let moveTo: string | undefined;
			if (index < lines.length - 1 && lines[index]!.startsWith("*** Move to: ")) {
				moveTo = parseOperationPath(lines[index]!, "*** Move to: ");
				index++;
			}

			const bodyLines: string[] = [];
			while (index < lines.length - 1 && !isOperationHeader(lines[index]!)) {
				bodyLines.push(lines[index]!);
				index++;
			}
			operations.push(parseUpdatePatch(path, moveTo, bodyLines));
			continue;
		}

		throw new Error(`Unexpected patch line: ${line}`);
	}

	if (operations.length === 0) {
		throw new Error("Patch did not contain any file operations.");
	}

	return {
		operations,
		touchedPaths: operations.flatMap((operation) => {
			if (operation.type === "update" && operation.moveTo && operation.moveTo !== operation.path) {
				return [operation.path, operation.moveTo];
			}
			return [operation.path];
		}),
	};
}

async function pathExists(path: string): Promise<boolean> {
	try {
		await access(path, constants.F_OK);
		return true;
	} catch (error) {
		if ((error as NodeJS.ErrnoException).code === "ENOENT") {
			return false;
		}
		throw error;
	}
}

async function withFileMutationQueues<T>(paths: string[], fn: () => Promise<T>): Promise<T> {
	const uniquePaths = [...new Set(paths.map((path) => resolve(path)))].sort();
	let wrapped = fn;
	for (let index = uniquePaths.length - 1; index >= 0; index--) {
		const path = uniquePaths[index]!;
		const previous = wrapped;
		wrapped = () => withFileMutationQueue(path, previous);
	}
	return wrapped();
}

function createDiffHeader(title: string): string {
	return `@@ ${title}`;
}

export function createApplyPatchToolDefinition(
	cwd: string,
	options?: ApplyPatchToolOptions,
): ToolDefinition<typeof applyPatchSchema, ApplyPatchToolDetails | undefined> {
	const tracker = options?.tracker;

	return {
		name: "apply_patch",
		label: "apply_patch",
		description:
			"Apply a compact patch across one or more files. Read existing files first, then use apply_patch for Add File, Delete File, Update File, and Move to changes because it only sends changed hunks instead of full file contents.",
		promptSnippet: "Apply a patch across one or more files",
		promptGuidelines: [
			"Read every existing file with read before changing it with apply_patch, and re-read if it changed since the last read.",
			"Use apply_patch for multi-file changes or diff-shaped edits to existing files.",
			"Prefer apply_patch over write when you only need to send changed hunks.",
			"Use edit for small exact replacements inside one existing file.",
		],
		parameters: applyPatchSchema,
		async execute(_toolCallId, { patch }, signal) {
			const parsedPatch = parseApplyPatch(patch);
			const resolvedTouchedPaths = parsedPatch.touchedPaths.map((path) => resolveToCwd(path, cwd));
			const seenPaths = new Set<string>();
			const duplicatePaths = new Set<string>();
			for (const path of resolvedTouchedPaths) {
				if (seenPaths.has(path)) {
					duplicatePaths.add(path);
				}
				seenPaths.add(path);
			}
			if (duplicatePaths.size > 0) {
				throw new Error(`Patch touches the same file more than once: ${Array.from(duplicatePaths).join(", ")}`);
			}

			return withFileMutationQueues(resolvedTouchedPaths, async () => {
				type MoveTarget = { absolutePath: string; path: string };
				type PreparedChange =
					| { type: "add"; absolutePath: string; content: string; path: string }
					| { type: "delete"; absolutePath: string; path: string }
					| { type: "update"; absolutePath: string; content: string; path: string; moveTarget?: MoveTarget };

				const preparedChanges: PreparedChange[] = [];
				const diffSections: string[] = [];
				const changedFiles: string[] = [];
				let addedCount = 0;
				let deletedCount = 0;
				let updatedCount = 0;
				let movedCount = 0;

				for (const operation of parsedPatch.operations) {
					if (signal?.aborted) {
						throw new Error("Operation aborted");
					}

					if (operation.type === "add") {
						const absolutePath = resolveToCwd(operation.path, cwd);
						if (await pathExists(absolutePath)) {
							throw new Error(`File already exists: ${operation.path}`);
						}
						diffSections.push(`${createDiffHeader(operation.path)}\n${generateDiffString("", normalizeToLF(operation.content)).diff}`);
						changedFiles.push(operation.path);
						preparedChanges.push({
							type: "add",
							absolutePath,
							content: operation.content,
							path: operation.path,
						});
						addedCount++;
						continue;
					}

					if (operation.type === "delete") {
						const absolutePath = resolveToCwd(operation.path, cwd);
						if (!(await pathExists(absolutePath))) {
							throw new Error(`File not found: ${operation.path}`);
						}
						const oldRawContent = (await readFile(absolutePath)).toString("utf-8");
						tracker?.assertFreshRead(
							absolutePath,
							operation.path,
							fingerprintFileContent(oldRawContent),
							"apply_patch",
						);
						diffSections.push(`${createDiffHeader(operation.path)}\n${generateDiffString(normalizeToLF(oldRawContent), "").diff}`);
						changedFiles.push(operation.path);
						preparedChanges.push({ type: "delete", absolutePath, path: operation.path });
						deletedCount++;
						continue;
					}

					const absolutePath = resolveToCwd(operation.path, cwd);
					if (!(await pathExists(absolutePath))) {
						throw new Error(`File not found: ${operation.path}`);
					}

					const rawContent = (await readFile(absolutePath)).toString("utf-8");
					tracker?.assertFreshRead(absolutePath, operation.path, fingerprintFileContent(rawContent), "apply_patch");
					const { bom, text: content } = stripBom(rawContent);
					const originalEnding = detectLineEnding(content);
					const normalizedContent = normalizeToLF(content);
					const { baseContent, newContent } =
						operation.edits.length > 0
							? applyEditsToNormalizedContent(normalizedContent, operation.edits, operation.path)
							: { baseContent: normalizedContent, newContent: normalizedContent };
					const finalContent = bom + restoreLineEndings(newContent, originalEnding);
					let moveTarget: MoveTarget | undefined;

					if (operation.moveTo && operation.moveTo !== operation.path) {
						const absoluteMoveTarget = resolveToCwd(operation.moveTo, cwd);
						if (await pathExists(absoluteMoveTarget)) {
							throw new Error(`Move target already exists: ${operation.moveTo}`);
						}
						moveTarget = { absolutePath: absoluteMoveTarget, path: operation.moveTo };
						diffSections.push(
							`${createDiffHeader(`${operation.path} -> ${operation.moveTo}`)}\n${generateDiffString(baseContent, newContent).diff}`,
						);
						changedFiles.push(operation.path, operation.moveTo);
						updatedCount++;
						movedCount++;
					} else {
						diffSections.push(`${createDiffHeader(operation.path)}\n${generateDiffString(baseContent, newContent).diff}`);
						changedFiles.push(operation.path);
						updatedCount++;
					}

					preparedChanges.push({
						type: "update",
						absolutePath,
						content: finalContent,
						path: operation.path,
						moveTarget,
					});
				}

				for (const change of preparedChanges) {
					if (signal?.aborted) {
						throw new Error("Operation aborted");
					}

					if (change.type === "add") {
						await mkdir(dirname(change.absolutePath), { recursive: true });
						await writeFile(change.absolutePath, change.content, "utf-8");
						tracker?.recordMutation(change.absolutePath, fingerprintFileContent(change.content), true);
						continue;
					}
					if (change.type === "delete") {
						await rm(change.absolutePath);
						tracker?.forget(change.absolutePath);
						continue;
					}
					if (change.moveTarget) {
						const movePreservesFullContent = tracker?.knowsFullContent(change.absolutePath) === true;
						await mkdir(dirname(change.moveTarget.absolutePath), { recursive: true });
						await writeFile(change.moveTarget.absolutePath, change.content, "utf-8");
						await rm(change.absolutePath);
						tracker?.forget(change.absolutePath);
						tracker?.recordMutation(
							change.moveTarget.absolutePath,
							fingerprintFileContent(change.content),
							movePreservesFullContent,
						);
						continue;
					}

					await writeFile(change.absolutePath, change.content, "utf-8");
					tracker?.recordMutation(change.absolutePath, fingerprintFileContent(change.content));
				}

				const summaryParts = [
					updatedCount > 0 ? `${updatedCount} updated` : undefined,
					addedCount > 0 ? `${addedCount} added` : undefined,
					deletedCount > 0 ? `${deletedCount} deleted` : undefined,
					movedCount > 0 ? `${movedCount} moved` : undefined,
				].filter((part): part is string => part !== undefined);

				return {
					content: [{ type: "text", text: `Applied patch: ${summaryParts.length > 0 ? summaryParts.join(", ") : "no changes"}.` }],
					details: {
						diff: diffSections.join("\n"),
						changedFiles: [...new Set(changedFiles)],
					},
				};
			});
		},
	};
}
