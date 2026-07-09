// @vitest-environment jsdom

import { cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeAll, describe, expect, it, vi } from "vitest";
import { Inspector, RunBoardDelegationList } from "./panels.tsx";
import type { Delegation, SessionSnapshot } from "./types.ts";

beforeAll(() => {
	class ResizeObserver {
		observe() {}
		unobserve() {}
		disconnect() {}
	}
	vi.stubGlobal("ResizeObserver", ResizeObserver);
	HTMLElement.prototype.scrollIntoView ??= () => {};
	HTMLElement.prototype.hasPointerCapture ??= () => false;
	HTMLElement.prototype.setPointerCapture ??= () => {};
	HTMLElement.prototype.releasePointerCapture ??= () => {};
});

afterEach(() => cleanup());

function deferred<T>() {
	let resolve!: (value: T | PromiseLike<T>) => void;
	let reject!: (reason?: unknown) => void;
	const promise = new Promise<T>((resolvePromise, rejectPromise) => {
		resolve = resolvePromise;
		reject = rejectPromise;
	});
	return { promise, resolve, reject };
}

function delegation(overrides: Partial<Delegation> = {}): Delegation {
	return {
		delegation_id: "delegation-1",
		kind: "readonly_fanout",
		status: "running",
		workflow: null,
		label: "Review release",
		progress: { expected: 2, spawned: 2, terminal: 0, running: 2, failed: 0 },
		subagents: [
			{ id: "child-1", status: "running", activity: "running", role: "reviewer", subagent_type: "read_only" },
			{ id: "child-2", status: "queued", activity: "queued", role: null, subagent_type: "read_only" },
		],
		...overrides,
	};
}

function snapshot(): SessionSnapshot {
	return {
		session_id: "parent-1",
		project_id: null,
		outer_cwd: "/workspace",
		workspaces: [],
		activity: "idle",
		active_leaf_id: null,
		provider: { kind: "openai", model: "gpt-test" },
		metadata: {},
		pending_actions: [],
		queued_inputs: [],
		last_event_id: 1,
		server_time_ms: 1_700_000_000_000,
	};
}

function renderList({
	parentSessionId = "parent-1",
	delegations = [delegation()],
	selectedSessionId = null,
	subagentNames = new Map(),
	onSelectSession = vi.fn(),
	onCancelDelegation = vi.fn(async () => undefined),
	mutationBlockedReason = null,
}: {
	parentSessionId?: string;
	delegations?: Delegation[];
	selectedSessionId?: string | null;
	subagentNames?: ReadonlyMap<string, string>;
	onSelectSession?: (sessionId: string) => void;
	onCancelDelegation?: (parentSessionId: string, delegationId: string) => void | Promise<void>;
	mutationBlockedReason?: string | null;
} = {}) {
	return render(
		<RunBoardDelegationList
			parentSessionId={parentSessionId}
			delegations={delegations}
			showAllDelegations
			onToggleShowAllDelegations={() => undefined}
			selectedSessionId={selectedSessionId}
			subagentNames={subagentNames}
			onSelectSession={onSelectSession}
			onCancelDelegation={onCancelDelegation}
			mutationBlockedReason={mutationBlockedReason}
		/>,
	);
}

describe("minimal delegated agent outline", () => {
	it("uses the canonical short name as primary text and updates it while keeping role secondary", () => {
		const item = delegation({
			subagents: [{
				id: "child-1",
				status: "running",
				activity: "running",
				role: "reviewer",
				subagent_type: "read_only",
			}],
		});
		const view = renderList({
			delegations: [item],
			subagentNames: new Map([["child-1", "Review checkout"]]),
		});

		let row = screen.getByRole("button", {
			name: "Open agent Review checkout, reviewer, running",
		});
		expect(within(row).getByText("Review checkout").className).toBe("run-board-subagent-name");
		expect(within(row).getByText("reviewer").className).toBe("run-board-subagent-role");
		expect(row.textContent).toBe("Review checkoutreviewer");

		view.rerender(
			<RunBoardDelegationList
				parentSessionId="parent-1"
				delegations={[item]}
				subagentNames={new Map([["child-1", "Review payments"]])}
				showAllDelegations
				onToggleShowAllDelegations={() => undefined}
				onCancelDelegation={() => undefined}
			/>,
		);

		row = screen.getByRole("button", {
			name: "Open agent Review payments, reviewer, running",
		});
		expect(within(row).getByText("Review payments")).toBeTruthy();
		expect(screen.queryByText("Review checkout")).toBeNull();
	});

	it("shows only task and role labels while accessible names convey shape-distinct status", async () => {
		const onSelectSession = vi.fn();
		const user = userEvent.setup();
		renderList({ selectedSessionId: "child-1", onSelectSession });

		const outline = document.querySelector(".run-board-outline");
		expect(outline?.textContent).toContain("Review release");
		expect(outline?.textContent).toContain("reviewer");
		expect(outline?.textContent).toContain("Agent");
		for (const forbidden of [
			"Writing task",
			"Parallel research",
			"Completed",
			"Done",
			"Activity idle",
			"expected",
			"spawned",
			"terminal",
			"running",
			"failed",
			"Outcome",
			"Needs attention",
			"Active",
			"Recent",
			"child-2",
		]) {
			expect(outline?.textContent).not.toContain(forbidden);
		}

		expect(screen.getAllByRole("img", { name: "running status" })).toHaveLength(2);
		expect(screen.getByRole("img", { name: "queued status" }).querySelector("svg")).toBeTruthy();
		const reviewer = screen.getByRole("button", { name: "Open agent Agent, reviewer, running" });
		expect(reviewer.getAttribute("aria-current")).toBe("page");
		await user.click(reviewer);
		expect(onSelectSession).toHaveBeenCalledWith("child-1");
	});

	it("uses Agent instead of a sliced technical id when role is unavailable", () => {
		renderList({
			delegations: [delegation({
				subagents: [{ id: "session_technical_123456789", status: "failed", activity: "idle", role: null }],
			})],
		});

		const row = screen.getByRole("button", { name: "Open agent Agent, failed" });
		expect(within(row).queryByText("reviewer")).toBeNull();
		expect(document.querySelector(".run-board-outline")?.textContent).not.toContain("session_technic");
	});

});

describe("cancel delegated work", () => {
	it("captures the parent target and confirms once even when the rendered parent changes", async () => {
		const pending = deferred<void>();
		const onCancel = vi.fn(() => pending.promise);
		const user = userEvent.setup();
		const view = renderList({
			parentSessionId: "parent-at-intent",
			delegations: [
				delegation(),
				delegation({ delegation_id: "delegation-2", label: "Other work" }),
			],
			onCancelDelegation: onCancel,
		});

		const work = screen.getByRole("article", { name: /Review release/ });
		await user.click(within(work).getByRole("button", { name: "Cancel" }));
		const dialog = screen.getByRole("alertdialog", { name: "Cancel delegated work?" });
		expect(within(dialog).getByText("Review release")).toBeTruthy();
		expect(within(dialog).getByText(/remaining work affecting 2 agents\/slots/)).toBeTruthy();
		expect(within(dialog).getByText(/cannot roll back external tool or network side effects/i)).toBeTruthy();
		await waitFor(() =>
			expect(document.activeElement).toBe(within(dialog).getByRole("button", { name: "Cancel" })),
		);

		view.rerender(
			<RunBoardDelegationList
				parentSessionId="parent-now-rendered"
				delegations={[delegation(), delegation({ delegation_id: "delegation-2", label: "Other work" })]}
				showAllDelegations
				onToggleShowAllDelegations={() => undefined}
				onCancelDelegation={onCancel}
			/>,
		);
		const confirm = screen.getByRole("button", { name: "Cancel work" });
		fireEvent.click(confirm);
		fireEvent.click(confirm);
		expect(onCancel).toHaveBeenCalledTimes(1);
		expect(onCancel).toHaveBeenCalledWith("parent-at-intent", "delegation-1");
		pending.resolve();
		await waitFor(() => expect(screen.queryByRole("alertdialog")).toBeNull());
	});

	it("scopes the spinner and duplicate lock to the selected task", async () => {
		const pending = deferred<void>();
		const onCancel = vi.fn(() => pending.promise);
		const user = userEvent.setup();
		renderList({
			delegations: [
				delegation(),
				delegation({ delegation_id: "delegation-2", label: "Other work" }),
			],
			onCancelDelegation: onCancel,
		});

		const work = screen.getByRole("article", { name: /Review release/ });
		const other = screen.getByRole("article", { name: /Other work/ });
		await user.click(within(work).getByRole("button", { name: "Cancel" }));
		const confirm = screen.getByRole("button", { name: "Cancel work" });
		fireEvent.click(confirm);
		fireEvent.click(confirm);

		expect(onCancel).toHaveBeenCalledTimes(1);
		expect(within(work).getByRole("button", { name: "Cancelling…", hidden: true }).getAttribute("aria-busy")).toBe("true");
		expect((within(other).getByRole("button", { name: "Cancel", hidden: true }) as HTMLButtonElement).disabled).toBe(false);

		pending.resolve();
		await waitFor(() => expect(screen.queryByRole("alertdialog")).toBeNull());
	});

	it("keeps an open intent offline and re-enables confirmation after reconnect", async () => {
		const onCancel = vi.fn(async () => undefined);
		const user = userEvent.setup();
		const view = renderList({ onCancelDelegation: onCancel });
		await user.click(screen.getByRole("button", { name: "Cancel" }));

		view.rerender(
			<RunBoardDelegationList
				parentSessionId="parent-1"
				delegations={[delegation()]}
				showAllDelegations
				onToggleShowAllDelegations={() => undefined}
				onCancelDelegation={onCancel}
				mutationBlockedReason="Waiting for connection"
			/>,
		);
		const blocked = screen.getByRole("button", { name: "Cancel work" }) as HTMLButtonElement;
		expect(blocked.disabled).toBe(true);
		expect(screen.getByRole("alertdialog").textContent).toContain("Waiting for connection");

		view.rerender(
			<RunBoardDelegationList
				parentSessionId="parent-1"
				delegations={[delegation()]}
				showAllDelegations
				onToggleShowAllDelegations={() => undefined}
				onCancelDelegation={onCancel}
			/>,
		);
		await user.click(screen.getByRole("button", { name: "Cancel work" }));
		expect(onCancel).toHaveBeenCalledTimes(1);
	});

	it("owns a persistent error and restores the compact action after failure", async () => {
		const onCancel = vi.fn(async () => {
			throw new Error("cancel request failed");
		});
		const user = userEvent.setup();
		renderList({ onCancelDelegation: onCancel });

		const work = screen.getByRole("article", { name: /Review release/ });
		await user.click(within(work).getByRole("button", { name: "Cancel" }));
		await user.click(screen.getByRole("button", { name: "Cancel work" }));
		await waitFor(() => expect(within(work).getByRole("alert").textContent).toContain("cancel request failed"));
		expect((within(work).getByRole("button", { name: "Cancel" }) as HTMLButtonElement).disabled).toBe(false);
	});
});

describe("agent list errors", () => {
	it("keeps Retry actionable only for the query failure", async () => {
		const onRetry = vi.fn();
		const user = userEvent.setup();
		const { rerender } = render(
			<Inspector
				snapshot={snapshot()}
				delegations={[]}
				delegationsLoading={false}
				delegationsError="list failed"
				onRetryDelegations={onRetry}
				onCancelDelegation={async () => undefined}
				tools={[]}
			/>,
		);

		expect(screen.getByRole("alert").textContent).toContain("Couldn’t load agents");
		await user.click(screen.getByRole("button", { name: "Retry" }));
		expect(onRetry).toHaveBeenCalledTimes(1);

		rerender(
			<Inspector
				snapshot={snapshot()}
				delegations={[]}
				delegationsLoading={false}
				delegationsError="list failed"
				delegationsRetrying
				onRetryDelegations={onRetry}
				onCancelDelegation={async () => undefined}
				tools={[]}
			/>,
		);
		expect((screen.getByRole("button", { name: "Retrying…" }) as HTMLButtonElement).disabled).toBe(true);
	});
});
