// @vitest-environment jsdom

import { act, cleanup, render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { MermaidBlock } from "./mermaidBlock.tsx";

const mermaidMock = vi.hoisted(() => {
	const api = {
		initialize: vi.fn(),
		parse: vi.fn(async () => true),
		render: vi.fn(async (_id: string, text: string) => ({
			svg: `<svg data-source="${text}"></svg>`,
		})),
	};
	return {
		api,
		moduleFactory: vi.fn(() => ({ default: api })),
	};
});

vi.mock("mermaid", mermaidMock.moduleFactory);

const mermaid = mermaidMock.api;

class MockIntersectionObserver implements IntersectionObserver {
	static instances: MockIntersectionObserver[] = [];

	readonly root: Element | Document | null;
	readonly rootMargin: string;
	readonly thresholds = [0];
	readonly callback: IntersectionObserverCallback;
	readonly observe = vi.fn();
	readonly unobserve = vi.fn();
	readonly disconnect = vi.fn();

	constructor(callback: IntersectionObserverCallback, options?: IntersectionObserverInit) {
		this.callback = callback;
		this.root = options?.root ?? null;
		this.rootMargin = options?.rootMargin ?? "0px";
		MockIntersectionObserver.instances.push(this);
	}

	takeRecords(): IntersectionObserverEntry[] {
		return [];
	}

	intersect(target: Element, isIntersecting = true) {
		this.callback([{ isIntersecting, target } as IntersectionObserverEntry], this);
	}
}

beforeEach(() => {
	MockIntersectionObserver.instances = [];
	vi.clearAllMocks();
	vi.stubGlobal("IntersectionObserver", MockIntersectionObserver);
});

afterEach(() => {
	cleanup();
	vi.unstubAllGlobals();
});

describe("MermaidBlock viewport rendering", () => {
	it("does not invoke Mermaid while the diagram remains offscreen", async () => {
		const { container } = render(<MermaidBlock code="flowchart LR; A --> B" />);
		const source = container.querySelector(".mermaid-source");
		const observer = MockIntersectionObserver.instances[0];

		expect(source).toBeTruthy();
		expect(observer?.rootMargin).toBe("600px 0px");
		expect(observer?.observe).toHaveBeenCalledWith(source);

		act(() => observer?.intersect(source!, false));
		await act(async () => {});

		expect(mermaidMock.moduleFactory).not.toHaveBeenCalled();
		expect(mermaid.parse).not.toHaveBeenCalled();
		expect(mermaid.render).not.toHaveBeenCalled();
		expect(observer?.disconnect).not.toHaveBeenCalled();
	});

	it("renders and stops observing when the diagram intersects", async () => {
		const { container } = render(
			<div className="message-scroll">
				<MermaidBlock code="flowchart LR; A --> B" />
			</div>,
		);
		const source = container.querySelector(".mermaid-source");
		const observer = MockIntersectionObserver.instances[0];

		expect(observer?.root).toBe(container.firstElementChild);
		act(() => observer?.intersect(source!));

		await waitFor(() => expect(screen.getByRole("img", { name: "Mermaid diagram" })).toBeTruthy());
		expect(mermaid.parse).toHaveBeenCalledWith("flowchart LR; A --> B", { suppressErrors: true });
		expect(mermaid.render).toHaveBeenCalledWith(expect.stringMatching(/^mermaid-svg-\d+$/), "flowchart LR; A --> B");
		expect(observer?.disconnect).toHaveBeenCalledTimes(1);
	});

	it("uses updated source on activation and renders later updates without observing again", async () => {
		const view = render(<MermaidBlock code="flowchart LR; A --> B" />);
		const source = view.container.querySelector(".mermaid-source");
		const observer = MockIntersectionObserver.instances[0];

		view.rerender(<MermaidBlock code="flowchart LR; B --> C" />);
		act(() => observer?.intersect(source!));

		await waitFor(() => expect(mermaid.render).toHaveBeenCalledTimes(1));
		expect(mermaid.render).toHaveBeenLastCalledWith(expect.any(String), "flowchart LR; B --> C");

		view.rerender(<MermaidBlock code="flowchart LR; C --> D" />);
		await waitFor(() => expect(mermaid.render).toHaveBeenCalledTimes(2));
		expect(mermaid.render).toHaveBeenLastCalledWith(expect.any(String), "flowchart LR; C --> D");
		expect(MockIntersectionObserver.instances).toHaveLength(1);
		expect(observer?.disconnect).toHaveBeenCalledTimes(1);
	});

	it("disconnects the observer when an offscreen diagram unmounts", () => {
		const view = render(<MermaidBlock code="flowchart LR; A --> B" />);
		const observer = MockIntersectionObserver.instances[0];

		view.unmount();

		expect(observer?.disconnect).toHaveBeenCalledTimes(1);
		expect(mermaid.render).not.toHaveBeenCalled();
	});

	it("renders immediately when IntersectionObserver is unavailable", async () => {
		vi.stubGlobal("IntersectionObserver", undefined);

		render(<MermaidBlock code="flowchart LR; A --> B" />);

		await waitFor(() => expect(screen.getByRole("img", { name: "Mermaid diagram" })).toBeTruthy());
		expect(mermaid.render).toHaveBeenCalledWith(expect.any(String), "flowchart LR; A --> B");
	});
});
