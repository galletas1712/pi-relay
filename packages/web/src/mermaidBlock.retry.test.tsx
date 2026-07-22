// @vitest-environment jsdom

import { cleanup, render, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

const mermaidMock = vi.hoisted(() => {
	const api = {
		initialize: vi.fn(),
		parse: vi.fn(async () => true),
		render: vi.fn(async (_id: string, text: string) => ({
			svg: `<svg data-source="${text}"></svg>`,
		})),
	};
	let importAttempts = 0;
	const moduleFactory = vi.fn(() => {
		importAttempts += 1;
		if (importAttempts === 1) throw new Error("Mermaid import failed");
		return { default: api };
	});
	return { api, moduleFactory };
});

vi.mock("mermaid", mermaidMock.moduleFactory);

let MermaidBlock: typeof import("./mermaidBlock.tsx").MermaidBlock;

beforeEach(async () => {
	// The production component intentionally keeps a module-level load promise.
	// Reset the module graph before importing it here so this retry test remains
	// independent when Vitest runs files with --no-isolate.
	vi.resetModules();
	({ MermaidBlock } = await import("./mermaidBlock.tsx"));
});

afterEach(cleanup);

describe("MermaidBlock Mermaid loading", () => {
	it("retries after the Mermaid import rejects", async () => {
		const view = render(<MermaidBlock code="flowchart LR; A --> B" />);

		await waitFor(() => expect(view.container.querySelector(".mermaid-source-error")).toBeTruthy());

		view.rerender(<MermaidBlock code="flowchart LR; B --> C" />);

		await waitFor(() => expect(mermaidMock.moduleFactory).toHaveBeenCalledTimes(2));
		await waitFor(() => expect(view.container.querySelector(".mermaid-diagram")).toBeTruthy());
		expect(mermaidMock.moduleFactory).toHaveBeenCalledTimes(2);
		expect(mermaidMock.api.render).toHaveBeenCalledWith(expect.any(String), "flowchart LR; B --> C");
	});
});
