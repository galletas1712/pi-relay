import { constants } from "node:fs";
import { access, open, readFile } from "node:fs/promises";
import type { ImageContent, TextContent } from "@mariozechner/pi-ai";
import type { ToolDefinition } from "@mariozechner/pi-coding-agent";
import { type Static, Type } from "@sinclair/typebox";
import { fileTypeFromBuffer } from "file-type";
import { type FileAccessTracker, fingerprintFileContent } from "./file-access-tracker.js";
import { resolveReadPath } from "./path-utils.js";
import { DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES, formatSize, type TruncationResult, truncateHead } from "./truncate.js";

const FILE_TYPE_SNIFF_BYTES = 4100;
const IMAGE_INLINE_BYTES_LIMIT = 1_500_000;
const IMAGE_MIME_TYPES = new Set(["image/jpeg", "image/png", "image/gif", "image/webp"]);

const readSchema = Type.Object(
	{
		path: Type.String({ description: "Path to the file to read (relative or absolute)" }),
		offset: Type.Optional(Type.Number({ description: "Line number to start reading from (1-indexed)" })),
		limit: Type.Optional(Type.Number({ description: "Maximum number of lines to read" })),
	},
	{ additionalProperties: false },
);

export type ReadToolInput = Static<typeof readSchema>;

export interface ReadToolDetails {
	truncation?: TruncationResult;
}

export interface ReadToolOptions {
	autoResizeImages?: boolean;
	tracker?: FileAccessTracker;
}

async function detectSupportedImageMimeTypeFromFile(filePath: string): Promise<string | null> {
	const fileHandle = await open(filePath, "r");
	try {
		const buffer = Buffer.alloc(FILE_TYPE_SNIFF_BYTES);
		const { bytesRead } = await fileHandle.read(buffer, 0, FILE_TYPE_SNIFF_BYTES, 0);
		if (bytesRead === 0) {
			return null;
		}

		const fileType = await fileTypeFromBuffer(buffer.subarray(0, bytesRead));
		if (!fileType || !IMAGE_MIME_TYPES.has(fileType.mime)) {
			return null;
		}
		return fileType.mime;
	} finally {
		await fileHandle.close();
	}
}

export function createReadToolDefinition(
	cwd: string,
	options?: ReadToolOptions,
): ToolDefinition<typeof readSchema, ReadToolDetails | undefined> {
	const autoResizeImages = options?.autoResizeImages ?? true;
	const tracker = options?.tracker;

	return {
		name: "read",
		label: "read",
		description: `Read the contents of a file. Supports text files and common images. Text output is truncated to ${DEFAULT_MAX_LINES} lines or ${DEFAULT_MAX_BYTES / 1024}KB. Use offset/limit for large files and continue with offset when you need the rest.`,
		promptSnippet: "Read file contents",
		promptGuidelines: ["Use read to examine files instead of cat or sed."],
		parameters: readSchema,
		async execute(_toolCallId, { path, offset, limit }, signal) {
			const absolutePath = resolveReadPath(path, cwd);
			if (signal?.aborted) {
				throw new Error("Operation aborted");
			}

			await access(absolutePath, constants.R_OK);
			if (signal?.aborted) {
				throw new Error("Operation aborted");
			}

			const mimeType = await detectSupportedImageMimeTypeFromFile(absolutePath);
			if (mimeType) {
				const buffer = await readFile(absolutePath);
				if (signal?.aborted) {
					throw new Error("Operation aborted");
				}
				if (autoResizeImages && buffer.length > IMAGE_INLINE_BYTES_LIMIT) {
					return {
						content: [
							{
								type: "text",
								text: `Read image file [${mimeType}]\n[Image omitted: relay read does not inline images above ${formatSize(IMAGE_INLINE_BYTES_LIMIT)}.]`,
							},
						],
						details: undefined,
					};
				}
				return {
					content: [
						{ type: "text", text: `Read image file [${mimeType}]` },
						{ type: "image", data: buffer.toString("base64"), mimeType },
					] satisfies (TextContent | ImageContent)[],
					details: undefined,
				};
			}

			const textContent = (await readFile(absolutePath)).toString("utf-8");
			if (signal?.aborted) {
				throw new Error("Operation aborted");
			}

			const fingerprint = fingerprintFileContent(textContent);
			const allLines = textContent.split("\n");
			const totalFileLines = allLines.length;
			const startLine = offset ? Math.max(0, offset - 1) : 0;
			const startLineDisplay = startLine + 1;
			if (startLine >= allLines.length) {
				throw new Error(`Offset ${offset} is beyond end of file (${allLines.length} lines total)`);
			}

			let selectedContent: string;
			let userLimitedLines: number | undefined;
			if (limit !== undefined) {
				const endLine = Math.min(startLine + limit, allLines.length);
				selectedContent = allLines.slice(startLine, endLine).join("\n");
				userLimitedLines = endLine - startLine;
			} else {
				selectedContent = allLines.slice(startLine).join("\n");
			}

			const truncation = truncateHead(selectedContent);
			let outputText: string;
			let details: ReadToolDetails | undefined;

			if (truncation.firstLineExceedsLimit) {
				const firstLineSize = formatSize(Buffer.byteLength(allLines[startLine] ?? "", "utf-8"));
				outputText = `[Line ${startLineDisplay} is ${firstLineSize}, exceeds ${formatSize(DEFAULT_MAX_BYTES)} limit. Use bash to inspect that line in smaller chunks.]`;
				details = { truncation };
			} else if (truncation.truncated) {
				const endLineDisplay = startLineDisplay + truncation.outputLines - 1;
				const nextOffset = endLineDisplay + 1;
				outputText = truncation.content;
				if (truncation.truncatedBy === "lines") {
					outputText += `\n\n[Showing lines ${startLineDisplay}-${endLineDisplay} of ${totalFileLines}. Use offset=${nextOffset} to continue.]`;
				} else {
					outputText += `\n\n[Showing lines ${startLineDisplay}-${endLineDisplay} of ${totalFileLines} (${formatSize(DEFAULT_MAX_BYTES)} limit). Use offset=${nextOffset} to continue.]`;
				}
				details = { truncation };
			} else if (userLimitedLines !== undefined && startLine + userLimitedLines < allLines.length) {
				const remaining = allLines.length - (startLine + userLimitedLines);
				const nextOffset = startLine + userLimitedLines + 1;
				outputText = `${truncation.content}\n\n[${remaining} more lines in file. Use offset=${nextOffset} to continue.]`;
			} else {
				outputText = truncation.content;
			}

			const deliveredFullContent =
				offset === undefined &&
				limit === undefined &&
				!truncation.truncated &&
				startLine === 0;
			if (deliveredFullContent && tracker?.shouldReturnCachedRead(absolutePath, fingerprint)) {
				outputText = `[File unchanged since the last full read: ${path}]`;
			}
			tracker?.recordRead(absolutePath, fingerprint, deliveredFullContent);

			return {
				content: [{ type: "text", text: outputText }] satisfies TextContent[],
				details,
			};
		},
	};
}
