import { describe, expect, it, vi } from "vitest";
import {
	concatBytes,
	downloadWorkspaceFile,
	mergeWorkspaceDirPage,
} from "./fileBrowser.ts";
import type { AgentApi } from "./agentApi.ts";

describe("mergeWorkspaceDirPage", () => {
	it("starts a listing from the first page", () => {
		expect(
			mergeWorkspaceDirPage(undefined, {
				path: "src",
				entries: [{ name: "a.rs", kind: "file" }],
				next_after_name: "a.rs",
			}),
		).toEqual({
			path: "src",
			entries: [{ name: "a.rs", kind: "file" }],
			next_after_name: "a.rs",
		});
	});

	it("appends later pages", () => {
		const merged = mergeWorkspaceDirPage(
			{
				path: "src",
				entries: [{ name: "a.rs", kind: "file" }],
				next_after_name: "a.rs",
			},
			{
				path: "src",
				entries: [{ name: "b.rs", kind: "file" }],
				next_after_name: null,
			},
		);
		expect(merged.entries.map((entry) => entry.name)).toEqual(["a.rs", "b.rs"]);
		expect(merged.next_after_name).toBeNull();
	});
});

describe("downloadWorkspaceFile", () => {
	it("loops ranged reads until eof", async () => {
		const reads: Array<{ offset?: number; maxBytes?: number }> = [];
		const api = {
			readWorkspaceFile: vi.fn(async (params) => {
				reads.push(params);
				if ((params.offset ?? 0) === 0) {
					return {
						path: "big.bin",
						content_base64: btoa("abcd"),
						byte_len: 4,
						total_size: 8,
						eof: false,
						mtime_ms: 1,
					};
				}
				return {
					path: "big.bin",
					content_base64: btoa("efgh"),
					byte_len: 4,
					total_size: 8,
					eof: true,
					mtime_ms: 1,
				};
			}),
		} as unknown as AgentApi;

		const file = await downloadWorkspaceFile(api, "session", "big.bin", 4);
		expect(reads.map((read) => read.offset)).toEqual([0, 4]);
		expect(new TextDecoder().decode(file.bytes)).toBe("abcdefgh");
		expect(file.totalSize).toBe(8);
	});
});

describe("concatBytes", () => {
	it("joins chunks", () => {
		expect(concatBytes([new Uint8Array([1, 2]), new Uint8Array([3])])).toEqual(
			new Uint8Array([1, 2, 3]),
		);
	});
});
