import { constants } from "node:fs";
import { access, readFile, writeFile } from "node:fs/promises";
import type { ToolDefinition } from "@pi-relay/coding-agent";
import { type Static, Type } from "@sinclair/typebox";
import {
	applyEditsToNormalizedContent,
	detectLineEnding,
	type Edit,
	generateDiffString,
	normalizeToLF,
	restoreLineEndings,
	stripBom,
} from "./edit-diff.js";
import { type FileAccessTracker, fingerprintFileContent } from "./file-access-tracker.js";
import { withFileMutationQueue } from "./file-mutation-queue.js";
import { resolveToCwd } from "./path-utils.js";

const replaceEditSchema = Type.Object(
	{
		oldText: Type.String({
			description:
				"Exact text for one targeted replacement. It must be unique in the original file and must not overlap with any other edits[].oldText in the same call.",
		}),
		newText: Type.String({ description: "Replacement text for this targeted edit." }),
	},
	{ additionalProperties: false },
);

const editSchema = Type.Object(
	{
		path: Type.String({ description: "Path to the file to edit (relative or absolute)" }),
		edits: Type.Array(replaceEditSchema, {
			description:
				"One or more targeted replacements. Each edit is matched against the original file, not incrementally. Do not include overlapping or nested edits. If two changes touch the same block or nearby lines, merge them into one edit instead.",
		}),
	},
	{ additionalProperties: false },
);

export type EditToolInput = Static<typeof editSchema>;

type LegacyEditToolInput = EditToolInput & {
	oldText?: unknown;
	newText?: unknown;
};

export interface EditToolDetails {
	diff: string;
	firstChangedLine?: number;
}

export interface EditToolOptions {
	tracker?: FileAccessTracker;
}

function prepareEditArguments(input: unknown): EditToolInput {
	if (!input || typeof input !== "object") {
		return input as EditToolInput;
	}

	const args = input as LegacyEditToolInput;
	if (typeof args.oldText !== "string" || typeof args.newText !== "string") {
		return input as EditToolInput;
	}

	const edits = Array.isArray(args.edits) ? [...args.edits] : [];
	edits.push({ oldText: args.oldText, newText: args.newText });
	const { oldText: _oldText, newText: _newText, ...rest } = args;
	return { ...rest, edits } as EditToolInput;
}

function validateEditInput(input: EditToolInput): { path: string; edits: Edit[] } {
	if (!Array.isArray(input.edits) || input.edits.length === 0) {
		throw new Error("Edit tool input is invalid. edits must contain at least one replacement.");
	}
	return { path: input.path, edits: input.edits };
}

export function createEditToolDefinition(
	cwd: string,
	options?: EditToolOptions,
): ToolDefinition<typeof editSchema, EditToolDetails | undefined> {
	const tracker = options?.tracker;

	return {
		name: "edit",
		label: "edit",
		description:
			"Edit a single file using exact text replacement. Every edits[].oldText must match a unique, non-overlapping region of the original file.",
		promptSnippet: "Make precise file edits with exact text replacement",
		promptGuidelines: [
			"Use edit for precise changes (edits[].oldText must match exactly).",
			"When changing multiple separate locations in one file, use one edit call with multiple entries in edits[].",
			"Keep edits[].oldText as small as possible while still being unique in the file.",
		],
		parameters: editSchema,
		prepareArguments: prepareEditArguments,
		async execute(_toolCallId, input, signal) {
			const { path, edits } = validateEditInput(input);
			const absolutePath = resolveToCwd(path, cwd);

			return withFileMutationQueue(absolutePath, async () => {
				if (signal?.aborted) {
					throw new Error("Operation aborted");
				}

				try {
					await access(absolutePath, constants.R_OK | constants.W_OK);
				} catch {
					throw new Error(`File not found: ${path}`);
				}
				if (signal?.aborted) {
					throw new Error("Operation aborted");
				}

				const rawContent = (await readFile(absolutePath)).toString("utf-8");
				const currentFingerprint = fingerprintFileContent(rawContent);
				tracker?.assertFreshRead(absolutePath, path, currentFingerprint, "edit");
				if (signal?.aborted) {
					throw new Error("Operation aborted");
				}

				const { bom, text: content } = stripBom(rawContent);
				const originalEnding = detectLineEnding(content);
				const normalizedContent = normalizeToLF(content);
				const { baseContent, newContent } = applyEditsToNormalizedContent(normalizedContent, edits, path);
				const finalContent = bom + restoreLineEndings(newContent, originalEnding);

				await writeFile(absolutePath, finalContent, "utf-8");
				tracker?.recordMutation(absolutePath, fingerprintFileContent(finalContent));
				if (signal?.aborted) {
					throw new Error("Operation aborted");
				}

				const diffResult = generateDiffString(baseContent, newContent);
				return {
					content: [{ type: "text", text: `Successfully replaced ${edits.length} block(s) in ${path}.` }],
					details: { diff: diffResult.diff, firstChangedLine: diffResult.firstChangedLine },
				};
			});
		},
	};
}
