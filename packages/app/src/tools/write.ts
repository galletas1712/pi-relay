import { constants } from "node:fs";
import { access, mkdir, readFile, writeFile } from "node:fs/promises";
import { dirname } from "node:path";
import type { ToolDefinition } from "@pi-relay/coding-agent";
import { type Static, Type } from "@sinclair/typebox";
import { type FileAccessTracker, fingerprintFileContent } from "./file-access-tracker.js";
import { withFileMutationQueue } from "./file-mutation-queue.js";
import { resolveToCwd } from "./path-utils.js";

const writeSchema = Type.Object(
	{
		path: Type.String({ description: "Path to the file to write (relative or absolute)" }),
		content: Type.String({ description: "Content to write to the file" }),
	},
	{ additionalProperties: false },
);

export type WriteToolInput = Static<typeof writeSchema>;

export interface WriteToolOptions {
	tracker?: FileAccessTracker;
}

export function createWriteToolDefinition(
	cwd: string,
	options?: WriteToolOptions,
): ToolDefinition<typeof writeSchema, undefined> {
	const tracker = options?.tracker;

	return {
		name: "write",
		label: "write",
		description: "Write content to a file. Creates the file if it does not exist, overwrites it if it does, and creates parent directories automatically.",
		promptSnippet: "Create or overwrite files",
		promptGuidelines: ["Use write only for new files or complete rewrites."],
		parameters: writeSchema,
		async execute(_toolCallId, { path, content }, signal) {
			const absolutePath = resolveToCwd(path, cwd);
			return withFileMutationQueue(absolutePath, async () => {
				if (signal?.aborted) {
					throw new Error("Operation aborted");
				}

				let existed = false;
				try {
					await access(absolutePath, constants.F_OK);
					existed = true;
				} catch (error) {
					if ((error as NodeJS.ErrnoException).code !== "ENOENT") {
						throw error;
					}
				}

				if (existed) {
					const currentContent = (await readFile(absolutePath)).toString("utf-8");
					tracker?.assertFreshRead(
						absolutePath,
						path,
						fingerprintFileContent(currentContent),
						"write",
						true,
					);
				}

				await mkdir(dirname(absolutePath), { recursive: true });
				if (signal?.aborted) {
					throw new Error("Operation aborted");
				}
				await writeFile(absolutePath, content, "utf-8");
				tracker?.recordMutation(absolutePath, fingerprintFileContent(content), true);
				if (signal?.aborted) {
					throw new Error("Operation aborted");
				}

				return {
					content: [{ type: "text", text: `Successfully wrote ${content.length} bytes to ${path}` }],
					details: undefined,
				};
			});
		},
	};
}
