import { describe, expect, it } from "vitest";
import { visibleBrowseDirectories } from "./filesTab.tsx";

describe("visibleBrowseDirectories", () => {
	it("always includes the cwd root", () => {
		expect(visibleBrowseDirectories([])).toEqual([""]);
	});

	it("adds expanded folder paths only", () => {
		expect(visibleBrowseDirectories(["src", "src/util"])).toEqual(["", "src", "src/util"]);
	});

	it("ignores the synthetic root item id", () => {
		expect(visibleBrowseDirectories(["__root__", "docs"])).toEqual(["", "docs"]);
	});
});
