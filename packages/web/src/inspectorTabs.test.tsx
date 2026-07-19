// @vitest-environment jsdom

import { fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it } from "vitest";
import { cleanup } from "@testing-library/react";
import { Inspector } from "./panels.tsx";

afterEach(cleanup);

describe("Inspector tab keyboard navigation", () => {
	it("uses roving tabIndex and activates Arrow, Home, and End destinations", () => {
		render(
			<Inspector
				snapshot={null}
				delegations={[]}
				delegationsLoading={false}
				delegationsError={null}
				onCancelDelegation={() => {}}
				tools={[]}
			/>,
		);
		const agents = screen.getByRole("tab", { name: "Agents" });
		const git = screen.getByRole("tab", { name: "Git" });
		const inspector = screen.getByRole("tab", { name: "Inspector" });
		expect(agents.tabIndex).toBe(0);
		expect(git.tabIndex).toBe(-1);
		expect(inspector.tabIndex).toBe(-1);

		agents.focus();
		fireEvent.keyDown(agents, { key: "ArrowRight" });
		expect(git.getAttribute("aria-selected")).toBe("true");
		expect(git.tabIndex).toBe(0);
		expect(document.activeElement).toBe(git);

		fireEvent.keyDown(git, { key: "End" });
		expect(inspector.getAttribute("aria-selected")).toBe("true");
		expect(document.activeElement).toBe(inspector);

		fireEvent.keyDown(inspector, { key: "Home" });
		expect(agents.getAttribute("aria-selected")).toBe("true");
		expect(document.activeElement).toBe(agents);

		fireEvent.keyDown(agents, { key: "ArrowLeft" });
		expect(inspector.getAttribute("aria-selected")).toBe("true");
		expect(document.activeElement).toBe(inspector);
	});
});

