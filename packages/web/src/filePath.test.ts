import { describe, expect, it } from "vitest";
import { bytesToUtf8Prefix, decodeBase64 } from "./fileBrowser.ts";
import {
	browsePathBasename,
	joinBrowsePath,
	parentBrowsePath,
	readFileQuery,
	validateBrowsePath,
} from "./filePath.ts";

describe("validateBrowsePath", () => {
	it("accepts root and normal relative paths", () => {
		expect(validateBrowsePath("")).toBe("");
		expect(validateBrowsePath("src/main.rs")).toBe("src/main.rs");
		expect(validateBrowsePath(".pi-handoff/note.md")).toBe(".pi-handoff/note.md");
	});

	it("rejects escapes and illegal forms", () => {
		expect(validateBrowsePath("..")).toBeNull();
		expect(validateBrowsePath("/abs")).toBeNull();
		expect(validateBrowsePath("a//b")).toBeNull();
		expect(validateBrowsePath("a/")).toBeNull();
		expect(validateBrowsePath("a\\b")).toBeNull();
	});
});

describe("browse path helpers", () => {
	it("joins and splits relative paths", () => {
		expect(joinBrowsePath("", "src")).toBe("src");
		expect(joinBrowsePath("src", "main.rs")).toBe("src/main.rs");
		expect(parentBrowsePath("src/main.rs")).toBe("src");
		expect(browsePathBasename("src/main.rs")).toBe("main.rs");
	});

	it("reads the file query param", () => {
		expect(readFileQuery("?file=docs%2Fspec.md")).toBe("docs/spec.md");
		expect(readFileQuery("?file=..")).toBeNull();
		expect(readFileQuery("")).toBeNull();
	});
});

describe("file browser decoding", () => {
	it("round-trips base64 and tolerates incomplete UTF-8 tails", () => {
		const encoded = btoa("hello");
		expect(Array.from(decodeBase64(encoded))).toEqual([104, 101, 108, 108, 111]);
		expect(bytesToUtf8Prefix(new TextEncoder().encode("ok")).text).toBe("ok");
		expect(bytesToUtf8Prefix(new Uint8Array([0x61, 0x00, 0x62])).binary).toBe(true);
	});
});
