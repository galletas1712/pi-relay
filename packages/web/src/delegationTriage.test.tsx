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
			{
				id: "child-1",
				status: "running",
				activity: "running",
				role: "reviewer",
				subagent_type: "read_only",
				task_prompt_file: "child-1/task_prompt.md",
			},
			{
				id: "child-2",
				status: "running",
				activity: "running",
				role: "tester",
				subagent_type: "read_only",
				task_prompt_file: "child-2/task_prompt.md",
			},
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
	onSelectSession = vi.fn(),
	onCancelDelegation = vi.fn(async () => undefined),
	onReRunDelegation = vi.fn(async () => undefined),
	mutationBlockedReason = null,
}: {
	parentSessionId?: string;
	delegations?: Delegation[];
	selectedSessionId?: string | null;
	onSelectSession?: (sessionId: string) => void;
	onCancelDelegation?: (parentSessionId: string, delegationId: string) => void | Promise<void>;
	onReRunDelegation?: (parentSessionId: string, delegation: Delegation) => void | Promise<void>;
	mutationBlockedReason?: string | null;
} = {}) {
	return render(
		<RunBoardDelegationList
			parentSessionId={parentSessionId}
			delegations={delegations}
			showAllDelegations
			onToggleShowAllDelegations={() => undefined}
			selectedSessionId={selectedSessionId}
			onSelectSession={onSelectSession}
			onCancelDelegation={onCancelDelegation}
			onReRunDelegation={onReRunDelegation}
			mutationBlockedReason={mutationBlockedReason}
		/>,
	);
}

describe("delegated agent navigation", () => {
	it("makes the full child row a stable selected navigation target with visible status and outcome", async () => {
		const onSelectSession = vi.fn();
		const user = userEvent.setup();
		renderList({
			selectedSessionId: "child-1",
			onSelectSession,
			delegations: [
				delegation({
					status: "done_with_failures",
					progress: { expected: 1, spawned: 1, terminal: 1, running: 0, failed: 1 },
					subagents: [{
						id: "child-1",
						status: "failed",
						activity: "idle",
						role: "reviewer",
						outcome: "changes_requested",
						task_prompt_file: "child-1/task_prompt.md",
					}],
				}),
			],
		});

		const row = screen.getByRole("button", {
			name: "Open agent reviewer, failed · activity idle, Failure: Changes requested",
		});
		expect(row.getAttribute("aria-current")).toBe("page");
		expect(within(row).getByText("failed · activity idle")).toBeTruthy();
		expect(within(row).getByText("Failure: Changes requested")).toBeTruthy();

		await user.click(within(row).getByText("Failure: Changes requested"));
		expect(onSelectSession).toHaveBeenCalledTimes(1);
		expect(onSelectSession).toHaveBeenCalledWith("child-1");
	});

	it("does not announce outcome text when the list contract supplies null", () => {
		renderList({
			selectedSessionId: "child-1",
			delegations: [
				delegation({
					status: "done",
					progress: { expected: 1, spawned: 1, terminal: 1, running: 0, failed: 0 },
					subagents: [{
						id: "child-1",
						status: "done",
						activity: "idle",
						role: "reviewer",
						task_prompt_file: "child-1/task_prompt.md",
						final_message_file: null,
						transcript_file: null,
						outcome: null,
					}],
				}),
			],
		});

		const row = screen.getByRole("button", {
			name: "Open agent reviewer, done · activity idle",
		});
		expect(row.getAttribute("aria-current")).toBe("page");
		expect(row.getAttribute("aria-label")).not.toMatch(/outcome|success/i);
	});
});

describe("cancel delegated work", () => {
	it("names affected work/count, focuses the safe action, confirms once, and exposes scoped pending", async () => {
		const pending = deferred<void>();
		const onCancel = vi.fn(() => pending.promise);
		const user = userEvent.setup();
		renderList({
			delegations: [
				delegation(),
				delegation({
					delegation_id: "delegation-2",
					label: "Other work",
				}),
			],
			onCancelDelegation: onCancel,
		});

		const work = screen.getByRole("article", { name: /Review release/ });
		const otherWork = screen.getByRole("article", { name: /Other work/ });
		await user.click(within(work).getByRole("button", { name: "Cancel" }));

		const dialog = screen.getByRole("alertdialog", { name: "Cancel delegated work?" });
		expect(within(dialog).getByText(/Review release/)).toBeTruthy();
		expect(within(dialog).getByText(/remaining work affecting 2 agents\/slots/)).toBeTruthy();
		expect(within(dialog).getByText(/cannot roll back external tool or network side effects/i)).toBeTruthy();
		await waitFor(() => expect(document.activeElement).toBe(within(dialog).getByRole("button", { name: "Cancel" })));

		const confirm = within(dialog).getByRole("button", { name: "Cancel work" });
		fireEvent.click(confirm);
		fireEvent.click(confirm);
		expect(onCancel).toHaveBeenCalledTimes(1);
		expect(onCancel).toHaveBeenCalledWith("parent-1", "delegation-1");
		expect(within(dialog).getByRole("button", { name: "Cancelling…" }).getAttribute("aria-busy")).toBe("true");
		expect(within(work).getByRole("button", { name: "Cancelling…", hidden: true })).toBeTruthy();
		expect((within(otherWork).getByRole("button", { name: "Cancel", hidden: true }) as HTMLButtonElement).disabled).toBe(false);

		pending.resolve();
		await waitFor(() => expect(screen.queryByRole("alertdialog")).toBeNull());
	});

	it("keeps the parent captured at intent when the rendered parent changes", async () => {
		const onCancel = vi.fn(async () => undefined);
		const user = userEvent.setup();
		const view = renderList({ parentSessionId: "parent-at-intent", onCancelDelegation: onCancel });

		await user.click(screen.getByRole("button", { name: "Cancel" }));
		view.rerender(
			<RunBoardDelegationList
				parentSessionId="parent-now-rendered"
				delegations={[delegation()]}
				showAllDelegations
				onToggleShowAllDelegations={() => undefined}
				onCancelDelegation={onCancel}
				onReRunDelegation={async () => undefined}
			/>,
		);
		await user.click(screen.getByRole("button", { name: "Cancel work" }));

		expect(onCancel).toHaveBeenCalledWith("parent-at-intent", "delegation-1");
	});

	it("keeps an open intent while offline, prevents RPC, and re-enables confirm after open", async () => {
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
				onReRunDelegation={async () => undefined}
				mutationBlockedReason="Waiting for connection"
			/>,
		);

		const dialog = screen.getByRole("alertdialog");
		const blockedConfirm = within(dialog).getByRole("button", { name: "Cancel work" }) as HTMLButtonElement;
		expect(blockedConfirm.disabled).toBe(true);
		expect(within(dialog).getByText("Waiting for connection")).toBeTruthy();
		fireEvent.click(blockedConfirm);
		expect(onCancel).not.toHaveBeenCalled();

		view.rerender(
			<RunBoardDelegationList
				parentSessionId="parent-1"
				delegations={[delegation()]}
				showAllDelegations
				onToggleShowAllDelegations={() => undefined}
				onCancelDelegation={onCancel}
				onReRunDelegation={async () => undefined}
			/>,
		);
		expect(screen.getByRole("alertdialog")).toBeTruthy();
		const enabledConfirm = screen.getByRole("button", { name: "Cancel work" }) as HTMLButtonElement;
		expect(enabledConfirm.disabled).toBe(false);
		await user.click(enabledConfirm);
		expect(onCancel).toHaveBeenCalledTimes(1);
	});

	it("closes after failure but keeps an owned error and restores controls for retry", async () => {
		const pending = deferred<void>();
		const onCancel = vi.fn(() => pending.promise);
		const user = userEvent.setup();
		renderList({ onCancelDelegation: onCancel });

		const work = screen.getByRole("article", { name: /Review release/ });
		await user.click(within(work).getByRole("button", { name: "Cancel" }));
		await user.click(screen.getByRole("button", { name: "Cancel work" }));
		pending.reject(new Error("cancel request failed"));

		await waitFor(() => expect(screen.queryByRole("alertdialog")).toBeNull());
		expect(within(work).getByRole("alert").textContent).toContain("cancel request failed");
		expect((within(work).getByRole("button", { name: "Cancel" }) as HTMLButtonElement).disabled).toBe(false);
	});
});

describe("re-run delegated work", () => {
	it("targets the explicit parent/delegation once and keeps pending scoped to the item", async () => {
		const pending = deferred<void>();
		const onReRun = vi.fn(() => pending.promise);
		const first = delegation({
			delegation_id: "finished-1",
			label: "Finished one",
			status: "done",
			progress: { expected: 1, spawned: 1, terminal: 1, running: 0, failed: 0 },
			subagents: [{
				id: "finished-child-1",
				status: "done",
				activity: "idle",
				role: "reviewer",
				task_prompt_file: "finished-child-1/task_prompt.md",
				outcome: "approved",
			}],
		});
		const second = delegation({
			delegation_id: "finished-2",
			label: "Finished two",
			status: "done",
			progress: { expected: 1, spawned: 1, terminal: 1, running: 0, failed: 0 },
			subagents: [{
				id: "finished-child-2",
				status: "done",
				activity: "idle",
				role: "tester",
				task_prompt_file: "finished-child-2/task_prompt.md",
				outcome: "pass",
			}],
		});
		renderList({ delegations: [first, second], onReRunDelegation: onReRun });

		const firstWork = screen.getByRole("article", { name: /Finished one/ });
		const secondWork = screen.getByRole("article", { name: /Finished two/ });
		const reRun = within(firstWork).getByRole("button", { name: "Re-run" });
		fireEvent.click(reRun);
		fireEvent.click(reRun);

		expect(onReRun).toHaveBeenCalledTimes(1);
		expect(onReRun).toHaveBeenCalledWith("parent-1", first);
		expect((within(firstWork).getByRole("button", { name: "Starting…" }) as HTMLButtonElement).disabled).toBe(true);
		expect((within(secondWork).getByRole("button", { name: "Re-run" }) as HTMLButtonElement).disabled).toBe(false);

		pending.resolve();
		await waitFor(() => expect(within(firstWork).getByRole("button", { name: "Re-run" })).toBeTruthy());
	});

	it("restores Re-run and keeps a persistent error after a failed start", async () => {
		const onReRun = vi.fn(async () => {
			throw new Error("start request failed");
		});
		const terminal = delegation({
			status: "failed",
			progress: { expected: 1, spawned: 1, terminal: 1, running: 0, failed: 1 },
			subagents: [{
				id: "failed-child",
				status: "failed",
				activity: "idle",
				role: "tester",
				task_prompt_file: "failed-child/task_prompt.md",
			}],
		});
		const user = userEvent.setup();
		renderList({ delegations: [terminal], onReRunDelegation: onReRun });

		const work = screen.getByRole("article", { name: /Review release/ });
		await user.click(within(work).getByRole("button", { name: "Re-run" }));
		await waitFor(() => expect(within(work).getByRole("alert").textContent).toContain("start request failed"));
		expect((within(work).getByRole("button", { name: "Re-run" }) as HTMLButtonElement).disabled).toBe(false);
	});
});

describe("agent list fetch errors", () => {
	function renderInspector(overrides: {
		delegations?: Delegation[];
		error: string;
		retrying?: boolean;
		onRetry?: () => void;
	}) {
		return render(
			<Inspector
				snapshot={snapshot()}
				delegations={overrides.delegations ?? []}
				delegationsLoading={false}
				delegationsError={overrides.error}
				delegationsRetrying={overrides.retrying}
				onRetryDelegations={overrides.onRetry}
				runBoard={{
					onCancelDelegation: async () => undefined,
					onReRunDelegation: async () => undefined,
				}}
				tools={[]}
			/>,
		);
	}

	it("offers an actionable canonical Retry when there is no data and reports busy state", async () => {
		const onRetry = vi.fn();
		const user = userEvent.setup();
		const { rerender } = renderInspector({ error: "list failed", onRetry });

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
				runBoard={{
					onCancelDelegation: async () => undefined,
					onReRunDelegation: async () => undefined,
				}}
				tools={[]}
			/>,
		);
		const retrying = screen.getByRole("button", { name: "Retrying…" }) as HTMLButtonElement;
		expect(retrying.disabled).toBe(true);
		expect(retrying.getAttribute("aria-busy")).toBe("true");
	});

	it("keeps cached work visible under a refresh warning", () => {
		renderInspector({
			delegations: [delegation()],
			error: "refresh failed",
			onRetry: vi.fn(),
		});

		expect(screen.getByRole("alert").textContent).toContain("Agent refresh failed");
		expect(screen.getByRole("article", { name: /Review release/ })).toBeTruthy();
	});
});
