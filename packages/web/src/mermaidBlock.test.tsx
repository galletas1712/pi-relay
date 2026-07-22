// @vitest-environment jsdom

import { act, cleanup, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
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
	mermaid.initialize.mockReset();
	mermaid.parse.mockReset().mockResolvedValue(true);
	mermaid.render.mockReset().mockImplementation(async (_id: string, text: string) => ({
		svg: `<svg data-source="${text}"></svg>`,
	}));
	mermaidMock.moduleFactory.mockReset().mockImplementation(() => ({ default: mermaid }));
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

		await waitFor(() => expect(screen.getByRole("button", { name: "Expand Mermaid diagram" })).toBeTruthy());
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

		await waitFor(() => expect(screen.getByRole("button", { name: "Expand Mermaid diagram" })).toBeTruthy());
		expect(mermaid.render).toHaveBeenCalledWith(expect.any(String), "flowchart LR; A --> B");
	});

	it("serializes parse and render operations across blocks", async () => {
		let resolveFirstParse!: (value: boolean) => void;
		const firstParse = new Promise<boolean>((resolve) => {
			resolveFirstParse = resolve;
		});
		let resolveFirstRender!: (value: { svg: string }) => void;
		const firstRender = new Promise<{ svg: string }>((resolve) => {
			resolveFirstRender = resolve;
		});
		mermaid.parse
			.mockImplementationOnce(() => firstParse)
			.mockImplementationOnce(async () => true);
		mermaid.render
			.mockImplementationOnce(() => firstRender)
			.mockImplementationOnce(async (_id: string, text: string) => ({
				svg: `<svg data-source="${text}"></svg>`,
			}));

		const { container } = render(
			<>
				<MermaidBlock code="flowchart LR; A --> B" />
				<MermaidBlock code="flowchart LR; C --> D" />
			</>,
		);
		const sources = container.querySelectorAll(".mermaid-source");
		const observers = MockIntersectionObserver.instances;
		act(() => {
			observers[0]?.intersect(sources[0]!);
			observers[1]?.intersect(sources[1]!);
		});

		await waitFor(() => expect(mermaid.parse).toHaveBeenCalledTimes(1));
		expect(mermaid.render).not.toHaveBeenCalled();

		resolveFirstParse(true);
		await waitFor(() => expect(mermaid.render).toHaveBeenCalledTimes(1));
		expect(mermaid.parse).toHaveBeenCalledTimes(1);
		// The queue must cover render() too, not only parse(). The second
		// operation cannot begin parsing while this promise is unresolved.
		expect(mermaid.render).toHaveBeenCalledTimes(1);

		resolveFirstRender({ svg: '<svg data-source="first"></svg>' });
		await waitFor(() => expect(mermaid.parse).toHaveBeenCalledTimes(2));
		await waitFor(() => expect(mermaid.render).toHaveBeenCalledTimes(2));
	});

	it("serializes theme initialization with parse and render across updates", async () => {
		const events: Array<{ kind: "initialize" | "parse" | "render"; theme?: string; code?: string }> = [];
		mermaid.initialize.mockImplementation((config) => {
			events.push({ kind: "initialize", theme: String(config.theme) });
		});
		mermaid.parse.mockImplementation(async (text?: string) => {
			events.push({ kind: "parse", code: text });
			return true;
		});

		let dark = false;
		const listeners = new Set<(event: MediaQueryListEvent) => void>();
		const media = {
			get matches() {
				return dark;
			},
			media: "(prefers-color-scheme: dark)",
			onchange: null,
			addEventListener: (_type: string, listener: (event: MediaQueryListEvent) => void) => {
				listeners.add(listener);
			},
			removeEventListener: (_type: string, listener: (event: MediaQueryListEvent) => void) => {
				listeners.delete(listener);
			},
			addListener: () => {},
			removeListener: () => {},
			dispatchEvent: () => true,
		};
		vi.stubGlobal("matchMedia", vi.fn(() => media));

		let resolveDefaultRender!: (value: { svg: string }) => void;
		const defaultRender = new Promise<{ svg: string }>((resolve) => {
			resolveDefaultRender = resolve;
		});
		let resolveDarkRender!: (value: { svg: string }) => void;
		const darkRender = new Promise<{ svg: string }>((resolve) => {
			resolveDarkRender = resolve;
		});
		mermaid.render
			.mockImplementationOnce(async (_id: string, text: string) => {
				events.push({ kind: "render", code: text });
				return defaultRender;
			})
			.mockImplementationOnce(async (_id: string, text: string) => {
				events.push({ kind: "render", code: text });
				return darkRender;
			})
			.mockImplementation(async (_id: string, text: string) => {
				events.push({ kind: "render", code: text });
				return { svg: `<svg data-source="${text}"></svg>` };
			});

		const view = render(<MermaidBlock code="flowchart LR; default --> A" />);
		const source = view.container.querySelector(".mermaid-source");
		const observer = MockIntersectionObserver.instances[0];
		act(() => observer?.intersect(source!));

		await waitFor(() => expect(mermaid.parse).toHaveBeenCalledTimes(1));
		await waitFor(() => expect(mermaid.render).toHaveBeenCalledTimes(1));
		const firstDefaultParseIndex = events.findIndex(
			({ kind, code }) => kind === "parse" && code === "flowchart LR; default --> A",
		);
		const firstDefaultRenderIndex = events.findIndex(
			({ kind, code }) => kind === "render" && code === "flowchart LR; default --> A",
		);
		expect(firstDefaultParseIndex).toBeLessThan(firstDefaultRenderIndex);
		const initialDefaultInitializeIndex = events.findIndex(
			({ kind, theme }) => kind === "initialize" && theme === "default",
		);
		if (initialDefaultInitializeIndex !== -1) {
			expect(initialDefaultInitializeIndex).toBeLessThan(firstDefaultParseIndex);
		}

		act(() => {
			dark = true;
			for (const listener of listeners) listener({ matches: true } as MediaQueryListEvent);
		});
		view.rerender(<MermaidBlock code="flowchart LR; dark --> B" />);
		await act(async () => {});
		expect(events).toHaveLength(firstDefaultRenderIndex + 1);

		resolveDefaultRender({ svg: '<svg data-source="default"></svg>' });
		await waitFor(() => expect(mermaid.parse).toHaveBeenCalledTimes(2));
		await waitFor(() => expect(mermaid.render).toHaveBeenCalledTimes(2));

		const darkInitializeIndex = events.findIndex(({ kind, theme }) => kind === "initialize" && theme === "dark");
		const darkParseIndex = events.findIndex(
			({ kind }, index) => kind === "parse" && index > firstDefaultRenderIndex,
		);
		const darkRenderIndex = events.findIndex(
			({ kind }, index) => kind === "render" && index > darkParseIndex,
		);
		expect(darkInitializeIndex).toBeGreaterThan(-1);
		expect(darkInitializeIndex).toBeLessThan(darkParseIndex);
		expect(darkParseIndex).toBeLessThan(darkRenderIndex);

		dark = false;
		act(() => {
			for (const listener of listeners) listener({ matches: false } as MediaQueryListEvent);
		});
		view.rerender(<MermaidBlock code="flowchart LR; default --> C" />);
		await act(async () => {});
		// The default operation is queued behind the unresolved dark render.
		expect(events).toHaveLength(darkRenderIndex + 1);

		resolveDarkRender({ svg: '<svg data-source="dark"></svg>' });
		await waitFor(() => expect(mermaid.parse).toHaveBeenCalledTimes(3));
		await waitFor(() => expect(mermaid.render).toHaveBeenCalledTimes(3));

		const finalDefaultInitializeIndex = events.reduce(
			(last, event, index) => event.kind === "initialize" && event.theme === "default" ? index : last,
			-1,
		);
		const finalDefaultParseIndex = events.reduce(
			(last, event, index) => event.kind === "parse" && event.code === "flowchart LR; default --> C" ? index : last,
			-1,
		);
		const finalDefaultRenderIndex = events.reduce(
			(last, event, index) => event.kind === "render" && event.code === "flowchart LR; default --> C" ? index : last,
			-1,
		);
		expect(finalDefaultInitializeIndex).toBeGreaterThan(darkRenderIndex);
		expect(finalDefaultInitializeIndex).toBeLessThan(finalDefaultParseIndex);
		expect(finalDefaultParseIndex).toBeLessThan(finalDefaultRenderIndex);
	});

	it.each([
		["parse", "parse failed"],
		["render", "render failed"],
	] as const)("continues processing after a %s rejection", async (failure, message) => {
		if (failure === "parse") {
			mermaid.parse.mockRejectedValueOnce(new Error(message));
		} else {
			mermaid.render.mockRejectedValueOnce(new Error(message));
		}

		const view = render(<MermaidBlock code="flowchart LR; failing --> A" />);
		const source = view.container.querySelector(".mermaid-source");
		act(() => MockIntersectionObserver.instances[0]?.intersect(source!));
		await waitFor(() => expect(view.container.querySelector(".mermaid-source-error")).toBeTruthy());

		view.rerender(<MermaidBlock code="flowchart LR; succeeding --> B" />);
		await waitFor(() => expect(screen.getByRole("button", { name: "Expand Mermaid diagram" })).toBeTruthy());

		expect(view.container.querySelector(".mermaid-source-error")).toBeNull();
		expect(mermaid.parse).toHaveBeenCalledTimes(2);
		expect(mermaid.render).toHaveBeenCalledTimes(failure === "parse" ? 1 : 2);
		expect(mermaid.render).toHaveBeenLastCalledWith(expect.any(String), "flowchart LR; succeeding --> B");
	});

	it("does not render stale parse work after a rerender", async () => {
		let resolveStaleParse!: (value: boolean) => void;
		mermaid.parse.mockImplementationOnce(
			() =>
				new Promise<boolean>((resolve) => {
					resolveStaleParse = resolve;
				}),
		);

		const view = render(<MermaidBlock code="flowchart LR; stale --> A" />);
		const source = view.container.querySelector(".mermaid-source");
		act(() => MockIntersectionObserver.instances[0]?.intersect(source!));
		await waitFor(() => expect(mermaid.parse).toHaveBeenCalledTimes(1));

		view.rerender(<MermaidBlock code="flowchart LR; current --> B" />);
		resolveStaleParse(true);
		await waitFor(() => expect(mermaid.parse).toHaveBeenCalledTimes(2));
		await waitFor(() => expect(mermaid.render).toHaveBeenCalledTimes(1));
		expect(mermaid.render).toHaveBeenLastCalledWith(expect.any(String), "flowchart LR; current --> B");
		expect(view.container.querySelector("[data-source*='stale']")).toBeNull();
		expect(view.container.querySelector("[data-source*='current']")).toBeTruthy();
	});

	it("does not replace current output with stale render work after a rerender", async () => {
		let resolveStaleRender!: (value: { svg: string }) => void;
		mermaid.render.mockImplementationOnce(
			() =>
				new Promise<{ svg: string }>((resolve) => {
					resolveStaleRender = resolve;
				}),
		);

		const view = render(<MermaidBlock code="flowchart LR; stale --> A" />);
		const source = view.container.querySelector(".mermaid-source");
		act(() => MockIntersectionObserver.instances[0]?.intersect(source!));
		await waitFor(() => expect(mermaid.render).toHaveBeenCalledTimes(1));

		view.rerender(<MermaidBlock code="flowchart LR; current --> B" />);
		resolveStaleRender({ svg: '<svg data-source="stale"></svg>' });
		await waitFor(() => expect(mermaid.render).toHaveBeenCalledTimes(2));
		expect(mermaid.render).toHaveBeenLastCalledWith(expect.any(String), "flowchart LR; current --> B");
		await waitFor(() => expect(view.container.querySelector("[data-source*='current']")).toBeTruthy());
		expect(view.container.querySelector("[data-source*='stale']")).toBeNull();
	});

	it("does not update an unmounted block after pending parse or render work settles", async () => {
		let resolveParse!: (value: boolean) => void;
		mermaid.parse.mockImplementationOnce(
			() =>
				new Promise<boolean>((resolve) => {
					resolveParse = resolve;
				}),
		);
		const parseView = render(<MermaidBlock code="flowchart LR; parse --> pending" />);
		const parseSource = parseView.container.querySelector(".mermaid-source");
		act(() => MockIntersectionObserver.instances[0]?.intersect(parseSource!));
		await waitFor(() => expect(mermaid.parse).toHaveBeenCalledTimes(1));
		parseView.unmount();
		await act(async () => {
			resolveParse(true);
			await Promise.resolve();
			await Promise.resolve();
			await Promise.resolve();
		});
		expect(mermaid.render).not.toHaveBeenCalled();

		let resolveRender!: (value: { svg: string }) => void;
		mermaid.render.mockImplementationOnce(
			() =>
				new Promise<{ svg: string }>((resolve) => {
					resolveRender = resolve;
				}),
		);
		const renderView = render(<MermaidBlock code="flowchart LR; render --> pending" />);
		const renderSource = renderView.container.querySelector(".mermaid-source");
		act(() => MockIntersectionObserver.instances.at(-1)?.intersect(renderSource!));
		await waitFor(() => expect(mermaid.render).toHaveBeenCalledTimes(1));
		renderView.unmount();
		resolveRender({ svg: '<svg data-source="unmounted"></svg>' });
		await act(async () => {});
		expect(renderView.container.innerHTML).toBe("");
	});
});

async function renderExpandedDiagram() {
	const user = userEvent.setup();
	const view = render(<MermaidBlock code="flowchart LR; A --> B" />);
	const source = view.container.querySelector(".mermaid-source");
	const observer = MockIntersectionObserver.instances[0];
	act(() => observer?.intersect(source!));
	const trigger = await screen.findByRole("button", { name: "Expand Mermaid diagram" });
	await user.click(trigger);
	await screen.findByRole("dialog", { name: "Mermaid diagram" });
	return { user, view, trigger };
}

describe("MermaidBlock expanded view", () => {
	it("opens a fullscreen dialog on click with a single SVG in the DOM", async () => {
		const { trigger } = await renderExpandedDiagram();

		expect(screen.getByRole("dialog", { name: "Mermaid diagram" })).toBeTruthy();
		expect(document.querySelectorAll(".mermaid-dialog-svg svg")).toHaveLength(1);
		expect(document.querySelectorAll(".mermaid-diagram-svg svg")).toHaveLength(0);
		expect(trigger.querySelector(".mermaid-diagram-placeholder")).toBeTruthy();
	});

	it("closes on Escape and restores the inline diagram", async () => {
		const { user, trigger } = await renderExpandedDiagram();

		await user.keyboard("{Escape}");
		await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
		expect(trigger.querySelector(".mermaid-diagram-svg svg")).toBeTruthy();
	});

	it("closes on the close button", async () => {
		const { user } = await renderExpandedDiagram();

		await user.click(screen.getByRole("button", { name: "close Mermaid diagram" }));
		await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
	});

	it("closes on overlay click", async () => {
		const { user } = await renderExpandedDiagram();

		await user.click(document.querySelector(".dialog-overlay") as HTMLElement);
		await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
	});
});
