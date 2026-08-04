// @vitest-environment jsdom

import { act, cleanup, render } from "@testing-library/react";
import { useCallback, useRef } from "react";
import { afterEach, describe, expect, it } from "vitest";
import { useParkedScrollPreservation } from "./parkedScroll.ts";

function ParkedScroller({ parked }: { parked: boolean }) {
	const scrollerRef = useRef<HTMLDivElement>(null);
	const getScroller = useCallback(() => scrollerRef.current, []);
	useParkedScrollPreservation(getScroller, parked);
	return (
		<div
			ref={scrollerRef}
			data-testid="scroller"
			style={{ height: 100, overflow: "auto" }}
		>
			<div style={{ height: 1000 }} />
		</div>
	);
}

function Harness({ parked }: { parked: boolean }) {
	return <ParkedScroller parked={parked} />;
}

describe("useParkedScrollPreservation", () => {
	afterEach(() => {
		cleanup();
	});

	it("restores scrollTop after an unpark transition", async () => {
		const view = render(<Harness parked={false} />);
		const scroller = document.querySelector<HTMLDivElement>("[data-testid='scroller']");
		expect(scroller).toBeTruthy();
		Object.defineProperty(scroller!, "scrollHeight", { configurable: true, get: () => 1000 });
		Object.defineProperty(scroller!, "clientHeight", { configurable: true, get: () => 100 });
		let scrollTop = 0;
		Object.defineProperty(scroller!, "scrollTop", {
			configurable: true,
			get: () => scrollTop,
			set: (value: number) => {
				scrollTop = value;
			},
		});

		scrollTop = 420;
		await act(async () => {
			view.rerender(<Harness parked={true} />);
		});
		// Simulate a layout clamp while parked.
		scrollTop = 0;
		await act(async () => {
			view.rerender(<Harness parked={false} />);
			await new Promise<void>((resolve) => requestAnimationFrame(() => resolve()));
		});
		expect(scrollTop).toBe(420);
	});
});
