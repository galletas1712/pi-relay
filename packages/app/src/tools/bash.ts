import { randomBytes } from "node:crypto";
import { createWriteStream } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import type { ToolDefinition } from "@mariozechner/pi-coding-agent";
import { createLocalBashOperations, type BashOperations } from "@mariozechner/pi-coding-agent";
import { type Static, Type } from "@sinclair/typebox";
import { DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES, formatSize, type TruncationResult, truncateTail } from "./truncate.js";

const bashSchema = Type.Object(
	{
		command: Type.String({ description: "Bash command to execute" }),
		timeout: Type.Optional(Type.Number({ description: "Timeout in seconds (optional, no default timeout)" })),
	},
	{ additionalProperties: false },
);

export type BashToolInput = Static<typeof bashSchema>;

export interface BashToolDetails {
	truncation?: TruncationResult;
	fullOutputPath?: string;
}

export interface BashToolOptions {
	operations?: BashOperations;
	commandPrefix?: string;
}

function getTempFilePath(): string {
	return join(tmpdir(), `pi-relay-bash-${randomBytes(8).toString("hex")}.log`);
}

export function createBashToolDefinition(
	cwd: string,
	options?: BashToolOptions,
): ToolDefinition<typeof bashSchema, BashToolDetails | undefined> {
	const operations = options?.operations ?? createLocalBashOperations();
	const commandPrefix = options?.commandPrefix;

	return {
		name: "bash",
		label: "bash",
		description: `Execute a bash command in the current working directory. Returns stdout and stderr. Output is truncated to the last ${DEFAULT_MAX_LINES} lines or ${DEFAULT_MAX_BYTES / 1024}KB. If truncated, the full output is saved to a temp file.`,
		promptSnippet: "Execute bash commands (ls, grep, find, etc.)",
		parameters: bashSchema,
		async execute(_toolCallId, { command, timeout }, signal, onUpdate) {
			const resolvedCommand = commandPrefix ? `${commandPrefix}\n${command}` : command;
			onUpdate?.({ content: [], details: undefined });

			return new Promise((resolve, reject) => {
				let tempFilePath: string | undefined;
				let tempFileStream: ReturnType<typeof createWriteStream> | undefined;
				let totalBytes = 0;
				const chunks: Buffer[] = [];
				let chunksBytes = 0;
				const maxChunksBytes = DEFAULT_MAX_BYTES * 2;

				const ensureTempFile = () => {
					if (tempFilePath) {
						return;
					}
					tempFilePath = getTempFilePath();
					tempFileStream = createWriteStream(tempFilePath);
					for (const chunk of chunks) {
						tempFileStream.write(chunk);
					}
				};

				const handleData = (data: Buffer) => {
					totalBytes += data.length;
					if (totalBytes > DEFAULT_MAX_BYTES) {
						ensureTempFile();
					}
					if (tempFileStream) {
						tempFileStream.write(data);
					}

					chunks.push(data);
					chunksBytes += data.length;
					while (chunksBytes > maxChunksBytes && chunks.length > 1) {
						const removed = chunks.shift()!;
						chunksBytes -= removed.length;
					}

					const fullText = Buffer.concat(chunks).toString("utf-8");
					const truncation = truncateTail(fullText);
					if (truncation.truncated) {
						ensureTempFile();
					}
					onUpdate?.({
						content: [{ type: "text", text: truncation.content || "" }],
						details: {
							truncation: truncation.truncated ? truncation : undefined,
							fullOutputPath: tempFilePath,
						},
					});
				};

				operations
					.exec(resolvedCommand, cwd, { onData: handleData, signal, timeout })
					.then(({ exitCode }) => {
						const fullOutput = Buffer.concat(chunks).toString("utf-8");
						const truncation = truncateTail(fullOutput);
						if (truncation.truncated) {
							ensureTempFile();
						}
						tempFileStream?.end();

						let outputText = truncation.content || "(no output)";
						let details: BashToolDetails | undefined;
						if (truncation.truncated) {
							details = { truncation, fullOutputPath: tempFilePath };
							const startLine = truncation.totalLines - truncation.outputLines + 1;
							const endLine = truncation.totalLines;
							if (truncation.lastLinePartial) {
								const lastLineSize = formatSize(Buffer.byteLength(fullOutput.split("\n").pop() || "", "utf-8"));
								outputText += `\n\n[Showing last ${formatSize(truncation.outputBytes)} of line ${endLine} (line is ${lastLineSize}). Full output: ${tempFilePath}]`;
							} else if (truncation.truncatedBy === "lines") {
								outputText += `\n\n[Showing lines ${startLine}-${endLine} of ${truncation.totalLines}. Full output: ${tempFilePath}]`;
							} else {
								outputText += `\n\n[Showing lines ${startLine}-${endLine} of ${truncation.totalLines} (${formatSize(DEFAULT_MAX_BYTES)} limit). Full output: ${tempFilePath}]`;
							}
						}

						if (exitCode !== 0 && exitCode !== null) {
							reject(new Error(`${outputText}\n\nCommand exited with code ${exitCode}`));
							return;
						}

						resolve({
							content: [{ type: "text", text: outputText }],
							details,
						});
					})
					.catch((error: Error) => {
						tempFileStream?.end();
						let output = Buffer.concat(chunks).toString("utf-8");
						if (error.message === "aborted") {
							output = output ? `${output}\n\nCommand aborted` : "Command aborted";
							reject(new Error(output));
							return;
						}
						if (error.message.startsWith("timeout:")) {
							const timeoutSeconds = error.message.split(":")[1];
							output = output ? `${output}\n\nCommand timed out after ${timeoutSeconds} seconds` : `Command timed out after ${timeoutSeconds} seconds`;
							reject(new Error(output));
							return;
						}
						reject(error);
					});
			});
		},
	};
}
