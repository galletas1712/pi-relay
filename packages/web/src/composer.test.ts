import { describe, expect, it } from "vitest";
import {
	isComposerFastSteerShortcut,
	isComposerSubmitShortcut,
} from "./composer.tsx";

describe("composer keyboard shortcuts", () => {
	it("treats Cmd/Ctrl+Enter without Shift as send", () => {
		expect(isComposerSubmitShortcut({ key: "Enter", metaKey: true, ctrlKey: false, shiftKey: false })).toBe(true);
		expect(isComposerSubmitShortcut({ key: "Enter", metaKey: false, ctrlKey: true, shiftKey: false })).toBe(true);
	});

	it("treats Cmd/Ctrl+Shift+Enter as fast steer, not ordinary send", () => {
		expect(isComposerFastSteerShortcut({ key: "Enter", metaKey: true, ctrlKey: false, shiftKey: true })).toBe(true);
		expect(isComposerFastSteerShortcut({ key: "Enter", metaKey: false, ctrlKey: true, shiftKey: true })).toBe(true);
		expect(isComposerSubmitShortcut({ key: "Enter", metaKey: true, ctrlKey: false, shiftKey: true })).toBe(false);
	});
});
