// @vitest-environment jsdom

import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { StrictMode, useCallback, useState, type ComponentProps } from "react";
import { afterEach, beforeAll, describe, expect, it, vi } from "vitest";
import {
	MessageList,
	clearAcknowledgedTranscriptDestination,
	removeLegacyTranscriptScroll,
	type OlderTurnsLoadRequest,
	type OlderTurnsLoadResult,
	type TranscriptDestination,
	type TurnCardView,
} from "./transcript.tsx";
import type { TranscriptEntry, TurnCard } from "./types.ts";

const resizeObservers: MockResizeObserver[] = [];

class MockResizeObserver implements ResizeObserver {
	readonly observed = new Set<Element>();
	disconnected = false;
	constructor(readonly callback: ResizeObserverCallback) {
		resizeObservers.push(this);
	}
	observe(target: Element) {
		this.observed.add(target);
	}
	unobserve(target: Element) {
		this.observed.delete(target);
	}
	disconnect() {
		this.disconnected = true;
		this.observed.clear();
	}
	trigger() {
		this.callback([], this);
	}
}

beforeAll(() => {
	vi.stubGlobal("ResizeObserver", MockResizeObserver);
});

afterEach(() => {
	cleanup();
	resizeObservers.length = 0;
	window.localStorage.clear();
});

describe("MessageList latest-on-entry scrolling", () => {
	it("waits for matching non-empty content, then initializes root and subagent sessions at latest", () => {
		const first = entry("root-entry", null, "root");
		const view = render(
			<MessageList {...props({
				entries: [],
				activeLeafId: null,
				sessionId: "root",
				entriesSessionId: null,
				loadingSession: true,
			})} />,
		);
		let scroller = messageScroller();
		mockScrollGeometry(scroller, 100, 1000);
		scroller.scrollTop = 137;

		view.rerender(
			<MessageList {...props({
				entries: [first],
				activeLeafId: first.id,
				sessionId: "root",
				entriesSessionId: "root",
			})} />,
		);
		expect(scroller.scrollTop).toBe(900);

		scroller.scrollTop = 211;
		fireEvent.scroll(scroller);
		view.rerender(
			<MessageList {...props({
				entries: [],
				activeLeafId: null,
				sessionId: "child",
				entriesSessionId: "root",
				loadingSession: true,
			})} />,
		);
		scroller = messageScroller();
		mockScrollGeometry(scroller, 100, 1200);
		const child = entry("child-entry", null, "child");
		view.rerender(
			<MessageList {...props({
				entries: [child],
				activeLeafId: child.id,
				sessionId: "child",
				entriesSessionId: "child",
			})} />,
		);

		expect(scroller.scrollTop).toBe(1100);
	});

	it("performs a Strict Mode destination initialization exactly once", () => {
		const original = entry("original", null, "original");
		const view = render(
			<StrictMode>
				<MessageList {...props({
					entries: [],
					activeLeafId: null,
					entriesSessionId: null,
					loadingSession: true,
				})} />
			</StrictMode>,
		);
		const scroller = messageScroller();
		mockScrollGeometry(scroller, 100, 1000);
		let scrollTop = 23;
		const writes: number[] = [];
		Object.defineProperty(scroller, "scrollTop", {
			configurable: true,
			get: () => scrollTop,
			set: (value: number) => {
				scrollTop = value;
				writes.push(value);
			},
		});

		view.rerender(
			<StrictMode>
				<MessageList {...props({
					entries: [original],
					activeLeafId: original.id,
				})} />
			</StrictMode>,
		);

		expect(writes).toEqual([900]);
		view.rerender(
			<StrictMode>
				<MessageList {...props({
					entries: [original],
					activeLeafId: original.id,
					serverTimeMs: 99,
					sessionError: "metadata refresh failed",
					sessionErrorHasUsableCache: true,
				})} />
			</StrictMode>,
		);
		expect(writes).toEqual([900]);
	});

	it.each([
		["ancestor", entry("ancestor", null, "ancestor")],
		["sibling", entry("sibling", null, "sibling")],
		["descendant", entry("descendant", "current", "descendant")],
	])("waits for the matching hydrated %s destination page, then initializes exactly once", (_kind, destinationEntry) => {
		const current = entry("current", null, "current");
		const view = render(
			<MessageList {...props({
				entries: [current],
				activeLeafId: current.id,
				turnPageIdentity: turnPageIdentity(current.id, 1),
			})} />,
		);
		const scroller = messageScroller();
		mockScrollGeometry(scroller, 100, 1000);
		let scrollTop = 250;
		const writes: number[] = [];
		Object.defineProperty(scroller, "scrollTop", {
			configurable: true,
			get: () => scrollTop,
			set: (value: number) => {
				scrollTop = value;
				writes.push(value);
			},
		});
		fireEvent.scroll(scroller);

		const destination = {
			id: 1,
			sessionId: "session",
			targetLeafId: destinationEntry.id,
			minimumTurnPageHydrationRevision: 2,
		};
		// App has committed history.switch metadata, but intentionally leaves
		// the previous turn-card page mounted while canonical hydration waits.
		view.rerender(
			<MessageList {...props({
				entries: [current],
				activeLeafId: current.id,
				destination,
				turnPageIdentity: turnPageIdentity(current.id, 1),
			})} />,
		);
		expect(writes).toEqual([]);
		activeResizeObserver().trigger();
		expect(writes).toEqual([]);

		view.rerender(
			<MessageList {...props({
				entries: [destinationEntry],
				activeLeafId: destinationEntry.id,
				destination,
				turnPageIdentity: turnPageIdentity(destinationEntry.id, 2),
			})} />,
		);
		expect(writes).toEqual([900]);

		view.rerender(
			<MessageList {...props({
				entries: [destinationEntry],
				activeLeafId: destinationEntry.id,
				destination,
				turnPageIdentity: turnPageIdentity(destinationEntry.id, 2),
				serverTimeMs: 2,
			})} />,
		);
		expect(writes).toEqual([900]);
	});

	it("defers remount initialization until a pending destination is ready, then parent-acknowledges it once", () => {
		const clientHeightSpy = vi.spyOn(HTMLElement.prototype, "clientHeight", "get")
			.mockImplementation(function (this: HTMLElement) {
				return this.classList.contains("message-scroll") ? 100 : 0;
			});
		let scrollHeight = 1000;
		const scrollHeightSpy = vi.spyOn(HTMLElement.prototype, "scrollHeight", "get")
			.mockImplementation(function (this: HTMLElement) {
				return this.classList.contains("message-scroll") ? scrollHeight : 0;
			});
		const current = entry("current", null, "current");
		const destinationEntry = entry("destination", null, "destination");
		const destination: TranscriptDestination = {
			id: 7,
			sessionId: "session",
			targetLeafId: destinationEntry.id,
			minimumTurnPageHydrationRevision: 2,
		};
		const acknowledgements: number[] = [];
		const view = render(
			<StrictMode>
				<DestinationHarness
					visible
					initialDestination={destination}
					entry={current}
					turnPageIdentity={turnPageIdentity(current.id, 1)}
					onAcknowledge={(id) => acknowledgements.push(id)}
				/>
			</StrictMode>,
		);

		// The first mount and a route-style remount both retain stale cards and
		// must defer ordinary latest-on-entry initialization.
		expect(messageScroller().scrollTop).toBe(0);
		view.rerender(
			<StrictMode>
				<DestinationHarness
					visible={false}
					initialDestination={destination}
					entry={current}
					turnPageIdentity={turnPageIdentity(current.id, 1)}
					onAcknowledge={(id) => acknowledgements.push(id)}
				/>
			</StrictMode>,
		);
		view.rerender(
			<StrictMode>
				<DestinationHarness
					visible
					initialDestination={destination}
					entry={current}
					turnPageIdentity={turnPageIdentity(current.id, 1)}
					onAcknowledge={(id) => acknowledgements.push(id)}
				/>
			</StrictMode>,
		);
		const remountedScroller = messageScroller();
		expect(remountedScroller.scrollTop).toBe(0);
		let remountedScrollTop = 0;
		const writes: number[] = [];
		Object.defineProperty(remountedScroller, "scrollTop", {
			configurable: true,
			get: () => remountedScrollTop,
			set: (value: number) => {
				remountedScrollTop = value;
				writes.push(value);
			},
		});

		view.rerender(
			<StrictMode>
				<DestinationHarness
					visible
					initialDestination={destination}
					entry={destinationEntry}
					turnPageIdentity={turnPageIdentity(destinationEntry.id, 2)}
					onAcknowledge={(id) => acknowledgements.push(id)}
				/>
			</StrictMode>,
		);
		expect(writes).toEqual([900]);
		expect(acknowledgements).toEqual([destination.id]);

		// The parent cleared the acknowledged ID. A later remount is an
		// ordinary latest-on-entry initialization, and ordinary leaf growth is
		// no longer suppressed by a retained destination.
		view.rerender(
			<StrictMode>
				<DestinationHarness
					visible={false}
					initialDestination={destination}
					entry={destinationEntry}
					turnPageIdentity={turnPageIdentity(destinationEntry.id, 2)}
					onAcknowledge={(id) => acknowledgements.push(id)}
				/>
			</StrictMode>,
		);
		view.rerender(
			<StrictMode>
				<DestinationHarness
					visible
					initialDestination={destination}
					entry={destinationEntry}
					turnPageIdentity={turnPageIdentity(destinationEntry.id, 2)}
					onAcknowledge={(id) => acknowledgements.push(id)}
				/>
			</StrictMode>,
		);
		const consumedRemountScroller = messageScroller();
		expect(consumedRemountScroller.scrollTop).toBe(900);
		scrollHeight = 1100;
		const ordinaryLeaf = entry("ordinary-leaf", destinationEntry.id, "ordinary");
		view.rerender(
			<StrictMode>
				<DestinationHarness
					visible
					initialDestination={destination}
					entry={ordinaryLeaf}
					turnPageIdentity={turnPageIdentity(ordinaryLeaf.id, 3)}
					onAcknowledge={(id) => acknowledgements.push(id)}
				/>
			</StrictMode>,
		);
		expect(consumedRemountScroller.scrollTop).toBe(1000);
		expect(acknowledgements).toEqual([destination.id]);

		clientHeightSpy.mockRestore();
		scrollHeightSpy.mockRestore();
	});

	it("ignores an abandoned destination after the route selects another session", () => {
		const current = entry("current", null, "current");
		const abandoned: TranscriptDestination = {
			id: 1,
			sessionId: "session",
			targetLeafId: "destination",
			minimumTurnPageHydrationRevision: 2,
		};
		const view = render(
			<MessageList {...props({
				entries: [current],
				activeLeafId: current.id,
				destination: abandoned,
				turnPageIdentity: turnPageIdentity(current.id, 1),
			})} />,
		);
		const scroller = messageScroller();
		mockScrollGeometry(scroller, 100, 1000);

		const next = entry("next-session", null, "next");
		view.rerender(
			<MessageList {...props({
				sessionId: "next-session",
				entriesSessionId: "next-session",
				entries: [next],
				activeLeafId: next.id,
				destination: abandoned,
				turnPageIdentity: {
					sessionId: "next-session",
					leafId: next.id,
					hydrationRevision: 1,
				},
			})} />,
		);
		expect(scroller.scrollTop).toBe(900);
	});

	it("clears only the acknowledged destination ID when a newer destination wins the race", () => {
		const oldDestination: TranscriptDestination = {
			id: 1,
			sessionId: "session",
			targetLeafId: "old",
			minimumTurnPageHydrationRevision: 2,
		};
		const newDestination: TranscriptDestination = {
			id: 2,
			sessionId: "session",
			targetLeafId: "new",
			minimumTurnPageHydrationRevision: 3,
		};
		expect(clearAcknowledgedTranscriptDestination(oldDestination, oldDestination.id)).toBeNull();
		expect(clearAcknowledgedTranscriptDestination(newDestination, oldDestination.id)).toBe(newDestination);
	});

	it("preserves the unchanged branch after a failed or no-op history switch", () => {
		const current = entry("current", null, "current");
		const view = render(
			<MessageList {...props({
				entries: [current],
				activeLeafId: current.id,
			})} />,
		);
		const scroller = messageScroller();
		mockScrollGeometry(scroller, 100, 1000);
		scroller.scrollTop = 250;
		fireEvent.scroll(scroller);

		view.rerender(
			<MessageList {...props({
				entries: [current],
				activeLeafId: current.id,
				serverTimeMs: 2,
			})} />,
		);

		expect(scroller.scrollTop).toBe(250);
		expect(screen.queryByText("Loading conversation…")).toBeNull();
	});

	it("preserves manual scroll on a sparse batched multi-descendant canonical refresh", () => {
		const first = entry("first", null, "first");
		const view = render(
			<MessageList {...props({
				entries: [first],
				activeLeafId: first.id,
			})} />,
		);
		const scroller = messageScroller();
		mockScrollGeometry(scroller, 100, 1200);
		scroller.scrollTop = 300;
		fireEvent.scroll(scroller);

		// The canonical tail page can omit intermediate descendant B and expose
		// only C. This is ordinary refresh growth unless App commits a destination.
		const sparseGrandchild = entry("third", "missing-second", "third");
		view.rerender(
			<MessageList {...props({
				entries: [sparseGrandchild],
				activeLeafId: sparseGrandchild.id,
			})} />,
		);

		expect(scroller.scrollTop).toBe(300);
	});

	it("follows ordinary leaf growth only while pinned", () => {
		const first = entry("first", null, "first");
		const view = render(
			<MessageList {...props({
				entries: [first],
				activeLeafId: first.id,
			})} />,
		);
		const scroller = messageScroller();
		const geometry = mockScrollGeometry(scroller, 100, 1000);
		scroller.scrollTop = 900;
		fireEvent.scroll(scroller);

		geometry.height = 1100;
		const second = entry("second", first.id, "second");
		view.rerender(
			<MessageList {...props({
				entries: [second],
				activeLeafId: second.id,
			})} />,
		);
		expect(scroller.scrollTop).toBe(1000);

		scroller.scrollTop = 300;
		fireEvent.scroll(scroller);
		geometry.height = 1200;
		const third = entry("third", "missing-intermediate", "third");
		view.rerender(
			<MessageList {...props({
				entries: [third],
				activeLeafId: third.id,
			})} />,
		);
		expect(scroller.scrollTop).toBe(300);
	});

	it("handles same-leaf ResizeObserver growth while pinned and unpinned", () => {
		const current = entry("current", null, "current");
		render(<MessageList {...props({ entries: [current], activeLeafId: current.id })} />);
		const scroller = messageScroller();
		const geometry = mockScrollGeometry(scroller, 100, 1000);
		scroller.scrollTop = 900;
		fireEvent.scroll(scroller);

		geometry.height = 1100;
		activeResizeObserver().trigger();
		expect(scroller.scrollTop).toBe(1000);

		scroller.scrollTop = 275;
		fireEvent.scroll(scroller);
		geometry.height = 1300;
		activeResizeObserver().trigger();
		expect(scroller.scrollTop).toBe(275);
	});
});

describe("MessageList older-page anchoring", () => {
	it("preserves a real visible card offset and excludes concurrent growth below the viewport", async () => {
		const pending = deferred<OlderTurnsLoadResult>();
		const onLoadOlderTurns = vi.fn((_: OlderTurnsLoadRequest) => pending.promise);
		const current = turnView("turn-2", 2);
		const older = turnView("turn-1", 1);
		const view = renderReadyTurns([current], onLoadOlderTurns);
		const scroller = messageScroller();
		const geometry = mockScrollGeometry(scroller, 100, 1000);
		const rects = mockTranscriptRects(scroller, { "turn-2": [20, 80] });
		scroller.scrollTop = 300;
		fireEvent.scroll(scroller);

		fireEvent.click(screen.getByRole("button", { name: "Load older turns" }));
		const request = onLoadOlderTurns.mock.calls[0][0];
		expect(request).toMatchObject({ requestId: 1, sessionId: "session" });

		// 100px was inserted above the visible card, while another 400px grew
		// below it. Offset anchoring must apply only the measured 100px.
		geometry.height = 1500;
		rects["turn-1"] = [-80, 10];
		rects["turn-2"] = [120, 180];
		view.rerender(<MessageList {...props({
			entries: [],
			activeLeafId: "finish-2",
			turnCards: [older, current],
			hasOlderTurns: true,
			onLoadOlderTurns,
			turnPageIdentity: turnPageIdentity("finish-2", 2),
		})} />);
		await act(async () => pending.resolve({
			...request,
			status: "committed",
			turnPageHydrationRevision: 2,
		}));

		expect(scroller.scrollTop).toBe(400);
	});

	it.each(["noop", "stale", "failed"] as const)("drops a %s older request without correcting the viewport", async (status) => {
		const pending = deferred<OlderTurnsLoadResult>();
		const onLoadOlderTurns = vi.fn((_: OlderTurnsLoadRequest) => pending.promise);
		renderReadyTurns([turnView("turn-2", 2)], onLoadOlderTurns);
		const scroller = messageScroller();
		mockScrollGeometry(scroller, 100, 1000);
		mockTranscriptRects(scroller, { "turn-2": [20, 80] });
		scroller.scrollTop = 300;
		fireEvent.scroll(scroller);
		fireEvent.click(screen.getByRole("button", { name: "Load older turns" }));
		const request = onLoadOlderTurns.mock.calls[0][0];

		await act(async () => pending.resolve({ ...request, status }));
		expect(scroller.scrollTop).toBe(300);
	});

	it("drops a rejected older request and allows a repeated load", async () => {
		const first = deferred<OlderTurnsLoadResult>();
		const second = deferred<OlderTurnsLoadResult>();
		const onLoadOlderTurns = vi.fn<(request: OlderTurnsLoadRequest) => Promise<OlderTurnsLoadResult>>()
			.mockImplementationOnce(() => first.promise)
			.mockImplementationOnce(() => second.promise);
		renderReadyTurns([turnView("turn-2", 2)], onLoadOlderTurns);
		const scroller = messageScroller();
		mockScrollGeometry(scroller, 100, 1000);
		mockTranscriptRects(scroller, { "turn-2": [20, 80] });
		scroller.scrollTop = 300;
		fireEvent.scroll(scroller);

		fireEvent.click(screen.getByRole("button", { name: "Load older turns" }));
		await act(async () => first.reject(new Error("older page failed")));
		expect(scroller.scrollTop).toBe(300);
		fireEvent.click(screen.getByRole("button", { name: "Load older turns" }));
		expect(onLoadOlderTurns).toHaveBeenCalledTimes(2);
		expect(onLoadOlderTurns.mock.calls[1][0].requestId).toBe(2);
		await act(async () => second.resolve({ ...onLoadOlderTurns.mock.calls[1][0], status: "noop" }));
	});

	it("cancels an in-flight anchor on leaf or session change", async () => {
		const pending = deferred<OlderTurnsLoadResult>();
		const onLoadOlderTurns = vi.fn((_: OlderTurnsLoadRequest) => pending.promise);
		const current = turnView("turn-2", 2);
		const view = renderReadyTurns([current], onLoadOlderTurns);
		const scroller = messageScroller();
		mockScrollGeometry(scroller, 100, 1000);
		mockTranscriptRects(scroller, { "turn-2": [20, 80] });
		scroller.scrollTop = 300;
		fireEvent.scroll(scroller);
		fireEvent.click(screen.getByRole("button", { name: "Load older turns" }));
		const request = onLoadOlderTurns.mock.calls[0][0];

		view.rerender(<MessageList {...props({
			entries: [],
			activeLeafId: "new-leaf",
			turnCards: [current],
			hasOlderTurns: true,
			onLoadOlderTurns,
		})} />);
		await act(async () => pending.resolve({
			...request,
			status: "committed",
			turnPageHydrationRevision: 2,
		}));
		expect(scroller.scrollTop).toBe(300);
	});

	it("corrects a committed duplicate or in-place card height change using the same real anchor", async () => {
		const pending = deferred<OlderTurnsLoadResult>();
		const onLoadOlderTurns = vi.fn((_: OlderTurnsLoadRequest) => pending.promise);
		const above = turnView("turn-1", 1);
		const current = turnView("turn-2", 2);
		const view = renderReadyTurns([above, current], onLoadOlderTurns);
		const scroller = messageScroller();
		const geometry = mockScrollGeometry(scroller, 100, 1000);
		const rects = mockTranscriptRects(scroller, {
			"turn-1": [-100, -10],
			"turn-2": [20, 80],
		});
		scroller.scrollTop = 300;
		fireEvent.scroll(scroller);
		fireEvent.click(screen.getByRole("button", { name: "Load older turns" }));
		const request = onLoadOlderTurns.mock.calls[0][0];

		geometry.height = 1400;
		rects["turn-2"] = [120, 180];
		view.rerender(<MessageList {...props({
			entries: [],
			activeLeafId: "finish-2",
			turnCards: [
				{ ...above, card: { ...above.card, summary: "updated in place" } },
				current,
			],
			hasOlderTurns: true,
			onLoadOlderTurns,
			turnPageIdentity: turnPageIdentity("finish-2", 2),
		})} />);
		await act(async () => pending.resolve({
			...request,
			status: "committed",
			turnPageHydrationRevision: 2,
		}));
		expect(scroller.scrollTop).toBe(400);
	});

	it("leaves the viewport unchanged for a true no-op", async () => {
		const pending = deferred<OlderTurnsLoadResult>();
		const onLoadOlderTurns = vi.fn((_: OlderTurnsLoadRequest) => pending.promise);
		renderReadyTurns([turnView("turn-2", 2)], onLoadOlderTurns);
		const scroller = messageScroller();
		mockScrollGeometry(scroller, 100, 1000);
		mockTranscriptRects(scroller, { "turn-2": [20, 80] });
		scroller.scrollTop = 300;
		fireEvent.scroll(scroller);
		fireEvent.click(screen.getByRole("button", { name: "Load older turns" }));
		const request = onLoadOlderTurns.mock.calls[0][0];

		await act(async () => pending.resolve({ ...request, status: "noop" }));
		expect(scroller.scrollTop).toBe(300);
	});

	it.each([
		["inserted committed page", "committed", "inserted"],
		["duplicate/in-place committed page", "committed", "duplicate"],
		["cursor-only committed page", "committed", "cursor"],
		["true no-op", "noop", "none"],
		["stale result", "stale", "none"],
		["failed result", "failed", "none"],
		["rejected request", "rejected", "none"],
	] as const)(
		"restores request-time pinned state after concurrent growth for a %s",
		async (_kind, status, committedKind) => {
			const pending = deferred<OlderTurnsLoadResult>();
			const onLoadOlderTurns = vi.fn((_: OlderTurnsLoadRequest) => pending.promise);
			const current = turnView("turn-2", 2);
			const view = renderReadyTurns([current], onLoadOlderTurns);
			const scroller = messageScroller();
			const geometry = mockScrollGeometry(scroller, 100, 1000);
			mockTranscriptRects(scroller, { "turn-2": [20, 80] });
			scroller.scrollTop = 900;
			fireEvent.scroll(scroller);
			fireEvent.click(screen.getByRole("button", { name: "Load older turns" }));
			const request = onLoadOlderTurns.mock.calls[0][0];

			geometry.height = 1200;
			activeResizeObserver().trigger();
			expect(scroller.scrollTop).toBe(900);

			if (status === "rejected") {
				await act(async () => pending.reject(new Error("older page rejected")));
			} else {
				await act(async () => pending.resolve({
					...request,
					status,
					turnPageHydrationRevision: status === "committed" ? 2 : undefined,
				}));
			}
			if (status === "committed") {
				// The request can settle before React commits the matching card
				// hydration. Pinned restoration waits for that exact revision.
				expect(scroller.scrollTop).toBe(900);
				view.rerender(<MessageList {...props({
					entries: [],
					activeLeafId: "finish-2",
					turnCards:
						committedKind === "inserted"
							? [turnView("turn-1", 1), current]
							: committedKind === "duplicate"
								? [{ ...current, card: { ...current.card, summary: "updated" } }]
								: [current],
					hasOlderTurns: true,
					onLoadOlderTurns,
					turnPageIdentity: turnPageIdentity("finish-2", 2),
				})} />);
			}

			expect(scroller.scrollTop).toBe(1100);
		},
	);

	it.each([
		["inserted success", "committed", "inserted", false],
		["committed duplicate update", "committed", "duplicate", false],
		["cursor-only commit", "committed", "cursor", false],
		["true no-op", "noop", "none", false],
		["stale", "stale", "none", false],
		["failed", "failed", "none", false],
		["rejected", "failed", "none", true],
	] as const)(
		"does not restore a pinned request-time viewport after manual scrolling during %s",
		async (_kind, status, committedKind, reject) => {
			const pending = deferred<OlderTurnsLoadResult>();
			const onLoadOlderTurns = vi.fn((_: OlderTurnsLoadRequest) => pending.promise);
			const current = turnView("turn-2", 2);
			const view = renderReadyTurns([current], onLoadOlderTurns);
			const scroller = messageScroller();
			mockScrollGeometry(scroller, 100, 1000);
			const rects = mockTranscriptRects(scroller, { "turn-2": [20, 80] });
			scroller.scrollTop = 900;
			fireEvent.scroll(scroller);
			fireEvent.click(screen.getByRole("button", { name: "Load older turns" }));
			const request = onLoadOlderTurns.mock.calls[0][0];

			fireEvent.wheel(scroller);
			scroller.scrollTop = 250;
			fireEvent.scroll(scroller);
			if (status === "committed") {
				rects["turn-2"] = [120, 180];
				view.rerender(<MessageList {...props({
					entries: [],
					activeLeafId: "finish-2",
					turnCards:
						committedKind === "inserted"
							? [turnView("turn-1", 1), current]
							: committedKind === "duplicate"
								? [{ ...current, card: { ...current.card, summary: "updated" } }]
								: [current],
					hasOlderTurns: true,
					onLoadOlderTurns,
					turnPageIdentity: turnPageIdentity("finish-2", 2),
				})} />);
			}

			await act(async () => {
				if (reject) pending.reject(new Error("older page rejected"));
				else pending.resolve({
					...request,
					status,
					turnPageHydrationRevision: status === "committed" ? 2 : undefined,
				});
			});
			expect(scroller.scrollTop).toBe(250);
		},
	);

	it("does not treat an unaccompanied browser or programmatic scroll event as user cancellation", async () => {
		const pending = deferred<OlderTurnsLoadResult>();
		const onLoadOlderTurns = vi.fn((_: OlderTurnsLoadRequest) => pending.promise);
		renderReadyTurns([turnView("turn-2", 2)], onLoadOlderTurns);
		const scroller = messageScroller();
		const geometry = mockScrollGeometry(scroller, 100, 1000);
		mockTranscriptRects(scroller, { "turn-2": [20, 80] });
		scroller.scrollTop = 900;
		fireEvent.scroll(scroller);
		fireEvent.click(screen.getByRole("button", { name: "Load older turns" }));
		const request = onLoadOlderTurns.mock.calls[0][0];

		geometry.height = 1200;
		scroller.scrollTop = 950;
		fireEvent.scroll(scroller);
		await act(async () => pending.resolve({ ...request, status: "noop" }));
		expect(scroller.scrollTop).toBe(1100);
	});

	it.each(["pinned", "unpinned"] as const)(
		"ignores content pointerdown with outside release before later programmatic scrolling for %s preservation",
		async (position) => {
			const pending = deferred<OlderTurnsLoadResult>();
			const onLoadOlderTurns = vi.fn((_: OlderTurnsLoadRequest) => pending.promise);
			const current = turnView("turn-2", 2);
			const view = renderReadyTurns([current], onLoadOlderTurns);
			const scroller = messageScroller();
			const geometry = mockScrollGeometry(scroller, 100, 1000);
			const rects = mockTranscriptRects(scroller, { "turn-2": [20, 80] });
			mockVerticalScrollbarGeometry(scroller);
			scroller.scrollTop = position === "pinned" ? 900 : 300;
			fireEvent.scroll(scroller);
			fireEvent.click(screen.getByRole("button", { name: "Load older turns" }));
			const request = onLoadOlderTurns.mock.calls[0][0];

			const content = scroller.querySelector<HTMLElement>("[data-transcript-anchor-id]");
			if (!content) throw new Error("missing transcript content");
			firePointer(content, "pointerdown", 11, 50);
			firePointer(window, "pointerup", 11, 50);

			geometry.height = position === "pinned" ? 1200 : 1400;
			scroller.scrollTop = position === "pinned" ? 950 : 320;
			fireEvent.scroll(scroller);
			if (position === "unpinned") {
				rects["turn-2"] = [120, 180];
				view.rerender(<MessageList {...props({
					entries: [],
					activeLeafId: "finish-2",
					turnCards: [turnView("turn-1", 1), current],
					hasOlderTurns: true,
					onLoadOlderTurns,
					turnPageIdentity: turnPageIdentity("finish-2", 2),
				})} />);
			}

			await act(async () => pending.resolve({
				...request,
				status: position === "pinned" ? "noop" : "committed",
				turnPageHydrationRevision: position === "unpinned" ? 2 : undefined,
			}));
			expect(scroller.scrollTop).toBe(position === "pinned" ? 1100 : 420);
		},
	);

	it.each(["pinned", "unpinned"] as const)(
		"cancels %s preservation when a gutter drag moves the viewport before outside release",
		async (position) => {
			const pending = deferred<OlderTurnsLoadResult>();
			const onLoadOlderTurns = vi.fn((_: OlderTurnsLoadRequest) => pending.promise);
			const current = turnView("turn-2", 2);
			const view = renderReadyTurns([current], onLoadOlderTurns);
			const scroller = messageScroller();
			const geometry = mockScrollGeometry(scroller, 100, 1000);
			const rects = mockTranscriptRects(scroller, { "turn-2": [20, 80] });
			mockVerticalScrollbarGeometry(scroller);
			scroller.scrollTop = position === "pinned" ? 900 : 300;
			fireEvent.scroll(scroller);
			fireEvent.click(screen.getByRole("button", { name: "Load older turns" }));
			const request = onLoadOlderTurns.mock.calls[0][0];

			firePointer(scroller, "pointerdown", 21, 95);
			scroller.scrollTop = 250;
			// Some browsers update scrollTop before delivering the native
			// scrollbar release and dispatch the scroll event afterward.
			firePointer(window, "pointerup", 21, 95);
			fireEvent.scroll(scroller);

			geometry.height = position === "pinned" ? 1200 : 1400;
			if (position === "unpinned") {
				rects["turn-2"] = [120, 180];
				view.rerender(<MessageList {...props({
					entries: [],
					activeLeafId: "finish-2",
					turnCards: [turnView("turn-1", 1), current],
					hasOlderTurns: true,
					onLoadOlderTurns,
					turnPageIdentity: turnPageIdentity("finish-2", 2),
				})} />);
			}
			await act(async () => pending.resolve({
				...request,
				status: position === "pinned" ? "noop" : "committed",
				turnPageHydrationRevision: position === "unpinned" ? 2 : undefined,
			}));
			expect(scroller.scrollTop).toBe(250);
		},
	);

	it.each(["outside release", "pointer cancel", "lost capture", "window blur"] as const)(
		"clears an unmoved gutter pointer on %s before later programmatic scrolling",
		async (end) => {
			const pending = deferred<OlderTurnsLoadResult>();
			const onLoadOlderTurns = vi.fn((_: OlderTurnsLoadRequest) => pending.promise);
			renderReadyTurns([turnView("turn-2", 2)], onLoadOlderTurns);
			const scroller = messageScroller();
			const geometry = mockScrollGeometry(scroller, 100, 1000);
			mockTranscriptRects(scroller, { "turn-2": [20, 80] });
			mockVerticalScrollbarGeometry(scroller);
			scroller.scrollTop = 900;
			fireEvent.scroll(scroller);
			fireEvent.click(screen.getByRole("button", { name: "Load older turns" }));
			const request = onLoadOlderTurns.mock.calls[0][0];

			firePointer(scroller, "pointerdown", 31, 95);
			if (end === "outside release") {
				firePointer(window, "pointerup", 31, 95);
			} else if (end === "pointer cancel") {
				firePointer(window, "pointercancel", 31, 95);
			} else if (end === "lost capture") {
				firePointer(scroller, "lostpointercapture", 31, 95);
			} else {
				window.dispatchEvent(new Event("blur"));
			}
			geometry.height = 1200;
			scroller.scrollTop = 950;
			fireEvent.scroll(scroller);

			await act(async () => pending.resolve({ ...request, status: "noop" }));
			expect(scroller.scrollTop).toBe(1100);
		},
	);

	it("does not let an old pointer release clear or contaminate a newer pointer and request", async () => {
		const first = deferred<OlderTurnsLoadResult>();
		const second = deferred<OlderTurnsLoadResult>();
		const onLoadOlderTurns = vi.fn<(request: OlderTurnsLoadRequest) => Promise<OlderTurnsLoadResult>>()
			.mockImplementationOnce(() => first.promise)
			.mockImplementationOnce(() => second.promise);
		renderReadyTurns([turnView("turn-2", 2)], onLoadOlderTurns);
		const scroller = messageScroller();
		const geometry = mockScrollGeometry(scroller, 100, 1000);
		mockTranscriptRects(scroller, { "turn-2": [20, 80] });
		mockVerticalScrollbarGeometry(scroller);
		scroller.scrollTop = 900;
		fireEvent.scroll(scroller);

		fireEvent.click(screen.getByRole("button", { name: "Load older turns" }));
		const firstRequest = onLoadOlderTurns.mock.calls[0][0];
		firePointer(scroller, "pointerdown", 41, 95);
		await act(async () => first.resolve({ ...firstRequest, status: "noop" }));

		fireEvent.click(screen.getByRole("button", { name: "Load older turns" }));
		const secondRequest = onLoadOlderTurns.mock.calls[1][0];
		firePointer(scroller, "pointerdown", 42, 95);
		firePointer(window, "pointerup", 41, 95);
		scroller.scrollTop = 250;
		fireEvent.scroll(scroller);
		firePointer(window, "pointerup", 42, 95);

		geometry.height = 1200;
		await act(async () => second.resolve({ ...secondRequest, status: "noop" }));
		expect(scroller.scrollTop).toBe(250);
	});

	it("removes temporary scrollbar pointer listeners on unmount", () => {
		const pending = deferred<OlderTurnsLoadResult>();
		const onLoadOlderTurns = vi.fn((_: OlderTurnsLoadRequest) => pending.promise);
		const view = renderReadyTurns([turnView("turn-2", 2)], onLoadOlderTurns);
		const scroller = messageScroller();
		mockScrollGeometry(scroller, 100, 1000);
		mockTranscriptRects(scroller, { "turn-2": [20, 80] });
		mockVerticalScrollbarGeometry(scroller);
		scroller.scrollTop = 900;
		fireEvent.scroll(scroller);
		fireEvent.click(screen.getByRole("button", { name: "Load older turns" }));
		const addListener = vi.spyOn(window, "addEventListener");
		const removeListener = vi.spyOn(window, "removeEventListener");
		try {
			firePointer(scroller, "pointerdown", 51, 95);
			const pointerUp = addListener.mock.calls.find(([type]) => type === "pointerup");
			const pointerCancel = addListener.mock.calls.find(([type]) => type === "pointercancel");
			const blur = addListener.mock.calls.find(([type]) => type === "blur");
			expect(pointerUp).toBeTruthy();
			expect(pointerCancel).toBeTruthy();
			expect(blur).toBeTruthy();

			view.unmount();

			expect(removeListener).toHaveBeenCalledWith("pointerup", pointerUp![1], true);
			expect(removeListener).toHaveBeenCalledWith("pointercancel", pointerCancel![1], true);
			expect(removeListener).toHaveBeenCalledWith("blur", blur![1]);
		} finally {
			addListener.mockRestore();
			removeListener.mockRestore();
		}
	});

	it.each(["wheel", "touch", "scrollbar", "keyboard"] as const)(
		"cancels pinned restoration on explicit %s scroll intent",
		async (intent) => {
			const pending = deferred<OlderTurnsLoadResult>();
			const onLoadOlderTurns = vi.fn((_: OlderTurnsLoadRequest) => pending.promise);
			renderReadyTurns([turnView("turn-2", 2)], onLoadOlderTurns);
			const scroller = messageScroller();
			const geometry = mockScrollGeometry(scroller, 100, 1000);
			mockTranscriptRects(scroller, { "turn-2": [20, 80] });
			mockVerticalScrollbarGeometry(scroller);
			scroller.scrollTop = 900;
			fireEvent.scroll(scroller);
			fireEvent.click(screen.getByRole("button", { name: "Load older turns" }));
			const request = onLoadOlderTurns.mock.calls[0][0];

			if (intent === "wheel") fireEvent.wheel(scroller);
			if (intent === "touch") fireEvent.touchMove(scroller);
			if (intent === "scrollbar") firePointer(scroller, "pointerdown", 61, 95);
			if (intent === "keyboard") {
				scroller.focus();
				fireEvent.keyDown(scroller, { key: "PageUp" });
			}
			geometry.height = 1200;
			scroller.scrollTop = 250;
			fireEvent.scroll(scroller);
			if (intent === "scrollbar") firePointer(window, "pointerup", 61, 95);

			await act(async () => pending.resolve({ ...request, status: "noop" }));
			expect(scroller.scrollTop).toBe(250);
		},
	);

	it("loads safely with zero-height or empty content when no visible anchor exists", async () => {
		const onLoadOlderTurns = vi.fn(async (request: OlderTurnsLoadRequest) => ({
			...request,
			status: "noop" as const,
		}));
		renderReadyTurns([turnView("turn-2", 2)], onLoadOlderTurns);
		const scroller = messageScroller();
		mockScrollGeometry(scroller, 0, 0);
		mockTranscriptRects(scroller, { "turn-2": [0, 0] });

		fireEvent.click(screen.getByRole("button", { name: "Load older turns" }));
		await waitFor(() => expect(onLoadOlderTurns).toHaveBeenCalledOnce());
		expect(scroller.scrollTop).toBe(0);
	});
});

function DestinationHarness({
	visible,
	initialDestination,
	entry: transcriptEntry,
	turnPageIdentity: identity,
	onAcknowledge,
}: {
	visible: boolean;
	initialDestination: TranscriptDestination;
	entry: TranscriptEntry;
	turnPageIdentity: NonNullable<ComponentProps<typeof MessageList>["turnPageIdentity"]>;
	onAcknowledge: (destinationId: number) => void;
}) {
	const [destination, setDestination] = useState<TranscriptDestination | null>(
		initialDestination,
	);
	const acknowledge = useCallback((destinationId: number) => {
		onAcknowledge(destinationId);
		setDestination((current) =>
			clearAcknowledgedTranscriptDestination(current, destinationId)
		);
	}, [onAcknowledge]);
	if (!visible) return null;
	return (
		<MessageList {...props({
			entries: [transcriptEntry],
			activeLeafId: transcriptEntry.id,
			destination,
			turnPageIdentity: identity,
			onAcknowledgeDestination: acknowledge,
		})} />
	);
}

describe("legacy transcript scroll cleanup", () => {
	it("removes preloaded legacy data without reading or restoring it", async () => {
		window.localStorage.setItem("piRelayTranscriptScroll:v1", "{\"positions\":{\"session\":{\"scrollTop\":42,\"sticky\":false}}}");
		render(<MessageList {...props({ entries: [entry("current", null, "current")], activeLeafId: "current" })} />);

		await waitFor(() => expect(window.localStorage.getItem("piRelayTranscriptScroll:v1")).toBeNull());
	});

	it("tolerates unavailable or throwing storage", () => {
		expect(() => removeLegacyTranscriptScroll(null)).not.toThrow();
		expect(() => removeLegacyTranscriptScroll({
			removeItem: () => {
				throw new Error("blocked");
			},
		})).not.toThrow();
	});
});

function renderReadyTurns(
	turnCards: TurnCardView[],
	onLoadOlderTurns: NonNullable<ComponentProps<typeof MessageList>["onLoadOlderTurns"]>,
) {
	return render(<MessageList {...props({
		entries: [],
		activeLeafId: turnCards.at(-1)?.card.active_leaf_id ?? null,
		turnCards,
		hasOlderTurns: true,
		onLoadOlderTurns,
		turnPageIdentity: turnPageIdentity(
			turnCards.at(-1)?.card.active_leaf_id ?? null,
			1,
		),
	})} />);
}

function props(overrides: Partial<ComponentProps<typeof MessageList>>): ComponentProps<typeof MessageList> {
	return {
		entries: [],
		activeLeafId: null,
		isRunning: false,
		serverTimeMs: null,
		hasSession: true,
		sessionId: "session",
		entriesSessionId: "session",
		...overrides,
	};
}

function messageScroller(): HTMLDivElement {
	const scroller = document.querySelector<HTMLDivElement>(".message-scroll");
	if (!scroller) throw new Error("missing message scroller");
	return scroller;
}

function mockScrollGeometry(node: HTMLElement, clientHeight: number, initialHeight: number) {
	const geometry = { height: initialHeight };
	Object.defineProperties(node, {
		clientHeight: { configurable: true, get: () => clientHeight },
		scrollHeight: { configurable: true, get: () => geometry.height },
	});
	return geometry;
}

function mockVerticalScrollbarGeometry(node: HTMLElement, scrollbarWidth = 15) {
	Object.defineProperties(node, {
		clientWidth: { configurable: true, value: 100 - scrollbarWidth },
		offsetWidth: { configurable: true, value: 100 },
		offsetHeight: { configurable: true, value: 100 },
	});
	vi.spyOn(node, "getBoundingClientRect").mockImplementation(() => rect(0, 100));
}

function firePointer(
	target: Element | Window,
	type: "pointerdown" | "pointerup" | "pointercancel" | "lostpointercapture",
	pointerId: number,
	clientX: number,
	clientY = 50,
) {
	const event = new MouseEvent(type, {
		bubbles: true,
		button: 0,
		clientX,
		clientY,
	});
	Object.defineProperties(event, {
		pointerId: { value: pointerId },
		pointerType: { value: "mouse" },
	});
	fireEvent(target, event);
}

function mockTranscriptRects(
	scroller: HTMLElement,
	initial: Record<string, [number, number]>,
): Record<string, [number, number]> {
	const rects = { ...initial };
	vi.spyOn(scroller, "getBoundingClientRect").mockImplementation(() => rect(0, scroller.clientHeight));
	for (const element of scroller.querySelectorAll<HTMLElement>("[data-transcript-anchor-id]")) {
		vi.spyOn(element, "getBoundingClientRect").mockImplementation(() => {
			const [top, bottom] = rects[element.dataset.transcriptAnchorId ?? ""] ?? [0, 0];
			return rect(top, bottom);
		});
	}
	const prototypeRect = HTMLElement.prototype.getBoundingClientRect;
	vi.spyOn(HTMLElement.prototype, "getBoundingClientRect").mockImplementation(function (this: HTMLElement) {
		if (this === scroller) return rect(0, scroller.clientHeight);
		const anchorId = this.dataset?.transcriptAnchorId;
		if (anchorId && rects[anchorId]) return rect(...rects[anchorId]);
		return prototypeRect.call(this);
	});
	return rects;
}

function rect(top: number, bottom: number): DOMRect {
	return {
		x: 0,
		y: top,
		top,
		bottom,
		left: 0,
		right: 100,
		width: 100,
		height: bottom - top,
		toJSON: () => ({}),
	};
}

function activeResizeObserver(): MockResizeObserver {
	const observer = [...resizeObservers].reverse().find((candidate) => !candidate.disconnected);
	if (!observer) throw new Error("missing active ResizeObserver");
	return observer;
}

function entry(id: string, parentId: string | null, text: string): TranscriptEntry {
	return {
		id,
		parent_id: parentId,
		timestamp_ms: 1,
		item: { type: "assistant_message", items: [{ type: "text", text }] },
	};
}

function turnView(id: string, turnId: number): TurnCardView {
	const card: TurnCard = {
		id,
		turn_id: turnId,
		status: "completed",
		outcome: "Graceful",
		start_entry_id: `start-${turnId}`,
		boundary_entry_id: `finish-${turnId}`,
		active_leaf_id: `finish-${turnId}`,
		start_sequence: turnId,
		end_sequence: turnId,
		start_timestamp_ms: turnId,
		timestamp_ms: turnId,
		user_messages: [{
			id: `user-${turnId}`,
			parent_id: `start-${turnId}`,
			timestamp_ms: turnId,
			item: { type: "user_message", content: [{ type: "text", text: `turn ${turnId}` }] },
		}],
		assistant_message: null,
		summary: null,
		can_resume: false,
	};
	return { card, entries: null, expanded: false, isCurrent: false };
}

function turnPageIdentity(leafId: string | null, hydrationRevision: number) {
	return {
		sessionId: "session",
		leafId,
		hydrationRevision,
	};
}

function deferred<T>() {
	let resolve!: (value: T) => void;
	let reject!: (error: unknown) => void;
	const promise = new Promise<T>((resolvePromise, rejectPromise) => {
		resolve = resolvePromise;
		reject = rejectPromise;
	});
	return { promise, resolve, reject };
}
