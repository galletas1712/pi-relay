// @vitest-environment jsdom

import { cleanup, render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { createRef } from "react";
import { afterEach, beforeAll, describe, expect, it, vi } from "vitest";
import { Composer, type ComposerHandle, QueuedInputPane, SlashMenu } from "./composer.tsx";
import {
	composerTextNeedsConnection,
	ConnectionRecoveryBanner,
	ConnectionRetryController,
	firstDisabledReason,
	remoteActionBlockedReason,
	WAITING_FOR_CONNECTION,
} from "./connectionRecovery.tsx";
import { DeleteSessionDialog, ProjectDialog, RenameSessionDialog, type ProjectDialogState } from "./entityDialogs.tsx";
import { CompactHistoryPickerDialog } from "./historyPickerCompact.tsx";
import { LogHeader, RunBoardDelegationList, SessionRow, sessionMenuItems } from "./panels.tsx";
import { COMMANDS } from "./slash.ts";
import { MessageList } from "./transcript.tsx";
import type { Delegation, QueuedInput, SessionSummary, TranscriptTreeNode } from "./types.ts";

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

afterEach(() => {
	cleanup();
	window.localStorage.clear();
});

describe("connection policy", () => {
	it.each(["connecting", "closed", "error"] as const)("blocks remote actions while %s", (status) => {
		expect(remoteActionBlockedReason(status)).toBe(WAITING_FOR_CONNECTION);
	});

	it("allows remote actions only when open and composes connection before operation reasons", () => {
		expect(remoteActionBlockedReason("open")).toBeNull();
		expect(firstDisabledReason(WAITING_FOR_CONNECTION, "Saving…")).toBe(WAITING_FOR_CONNECTION);
		expect(firstDisabledReason(null, "Saving…")).toBe("Saving…");
	});

	it.each([
		["plain text", true],
		["/compact", true],
		["/system", true],
		["/help", false],
		["/export", false],
		["/switch", true],
	] as const)("classifies %s composer input", (text, expected) => {
		expect(composerTextNeedsConnection(text)).toBe(expected);
	});

	it("classifies /switch as local only when canonical history is already cached", () => {
		expect(composerTextNeedsConnection("/switch", { cachedHistoryAvailable: true })).toBe(false);
		expect(composerTextNeedsConnection("/switch", { cachedHistoryAvailable: false })).toBe(true);
	});
});

describe("ConnectionRecoveryBanner", () => {
	it.each([
		["connecting", false, "Connecting…", false],
		["connecting", true, "Reconnecting…", false],
		["closed", true, "Connection closed", true],
		["error", true, "Connection error", true],
	] as const)("renders %s as %s", (status, hasConnected, title, hasRetry) => {
		render(
			<ConnectionRecoveryBanner
				status={status}
				hasConnected={hasConnected}
				retrying={false}
				onRetry={() => undefined}
			/>,
		);

		expect(screen.getByText(title)).toBeTruthy();
		expect(screen.getByRole("status").getAttribute("aria-live")).toBe("off");
		expect(screen.queryByRole("button", { name: "Retry connection" }) !== null).toBe(hasRetry);
	});

	it("shows a disabled busy Retry state and hides completely when open", () => {
		const { rerender } = render(
			<ConnectionRecoveryBanner status="closed" hasConnected retrying onRetry={() => undefined} />,
		);
		const retry = screen.getByRole("button", { name: "Retrying…" }) as HTMLButtonElement;
		expect(retry.disabled).toBe(true);
		expect(retry.getAttribute("aria-busy")).toBe("true");

		rerender(<ConnectionRecoveryBanner status="open" hasConnected retrying={false} onRetry={() => undefined} />);
		expect(screen.queryByRole("status")).toBeNull();
	});
});

describe("ConnectionRetryController", () => {
	it("deduplicates Retry and fences a late failure after a later open", async () => {
		const attempt = deferred<void>();
		const connect = vi.fn(() => attempt.promise);
		const onFailure = vi.fn();
		const onSettled = vi.fn();
		const controller = new ConnectionRetryController();

		const first = controller.retry(connect, onFailure, onSettled);
		const duplicate = controller.retry(connect, onFailure, onSettled);
		expect(first).toBe(duplicate);
		expect(connect).toHaveBeenCalledTimes(1);

		controller.opened();
		attempt.reject(new Error("stale failure"));
		await first;

		expect(onFailure).not.toHaveBeenCalled();
		expect(onSettled).not.toHaveBeenCalled();
		expect(controller.isPending()).toBe(false);
	});

	it("does not modify composer draft storage while retrying", async () => {
		window.localStorage.setItem("piRelayComposerDrafts:v1", JSON.stringify({
			drafts: { session_1: "keep this draft" },
		}));
		const attempt = deferred<void>();
		const controller = new ConnectionRetryController();

		const retry = controller.retry(() => attempt.promise, () => undefined, () => undefined);
		attempt.resolve();
		await retry;

		expect(window.localStorage.getItem("piRelayComposerDrafts:v1")).toContain("keep this draft");
	});
});

describe("offline drafting and composer gates", () => {
	it("keeps draft editing available, blocks Send and Stop visibly, and re-enables without changing the draft", async () => {
		const user = userEvent.setup();
		const onSubmit = vi.fn(async () => true);
		const handle = createRef<ComposerHandle>();
		const { rerender } = render(
			<Composer
				selectedId="session-1"
				selectedIsSubagent={false}
				composerHandleRef={handle}
				sending={false}
				canStop
				stopping={false}
				queuedInputs={[]}
				mutationBlockedReason={WAITING_FOR_CONNECTION}
				onSubmit={onSubmit}
				onStop={() => undefined}
				onPromoteQueued={() => undefined}
				onUpdateQueued={() => undefined}
				onCancelQueued={() => undefined}
				onMoveQueued={() => undefined}
			/>,
		);
		const composer = screen.getByRole("textbox");
		await user.type(composer, "offline draft");
		expect((composer as HTMLTextAreaElement).value).toBe("offline draft");
		expect((screen.getByRole("button", { name: "send message" }) as HTMLButtonElement).disabled).toBe(true);
		expect((screen.getByRole("button", { name: "stop active turn" }) as HTMLButtonElement).disabled).toBe(true);
		expect(screen.getByText(WAITING_FOR_CONNECTION).getAttribute("tabindex")).toBe("0");

		rerender(
			<Composer
				selectedId="session-1"
				selectedIsSubagent={false}
				composerHandleRef={handle}
				sending={false}
				canStop
				stopping={false}
				queuedInputs={[]}
				mutationBlockedReason={null}
				onSubmit={onSubmit}
				onStop={() => undefined}
				onPromoteQueued={() => undefined}
				onUpdateQueued={() => undefined}
				onCancelQueued={() => undefined}
				onMoveQueued={() => undefined}
			/>,
		);
		expect((screen.getByRole("textbox") as HTMLTextAreaElement).value).toBe("offline draft");
		expect((screen.getByRole("button", { name: "send message" }) as HTMLButtonElement).disabled).toBe(false);
	});

	it("leaves local Help and Export slash submissions available while remote slash rows are disabled", async () => {
		const user = userEvent.setup();
		const handle = createRef<ComposerHandle>();
		render(
			<Composer
				selectedId="session-1"
				selectedIsSubagent={false}
				composerHandleRef={handle}
				sending={false}
				canStop={false}
				stopping={false}
				queuedInputs={[]}
				mutationBlockedReason={WAITING_FOR_CONNECTION}
				onSubmit={() => true}
				onStop={() => undefined}
				onPromoteQueued={() => undefined}
				onUpdateQueued={() => undefined}
				onCancelQueued={() => undefined}
				onMoveQueued={() => undefined}
			/>,
		);
		await user.type(screen.getByRole("textbox"), "/help");
		expect((screen.getByRole("button", { name: "send message" }) as HTMLButtonElement).disabled).toBe(false);
		cleanup();

		render(
			<SlashMenu
				commands={COMMANDS}
				visible
				selectedIndex={0}
				mutationBlockedReason={WAITING_FOR_CONNECTION}
				cachedHistoryAvailable
				onSetIndex={() => undefined}
				onSelect={() => undefined}
			/>,
		);
		expect((screen.getByRole("option", { name: /help/i }) as HTMLButtonElement).disabled).toBe(false);
		expect((screen.getByRole("option", { name: /export/i }) as HTMLButtonElement).disabled).toBe(false);
		expect((screen.getByRole("option", { name: /switch/i }) as HTMLButtonElement).disabled).toBe(false);
		expect((screen.getByRole("option", { name: /compact/i }) as HTMLButtonElement).disabled).toBe(true);
		expect((screen.getByRole("option", { name: /system/i }) as HTMLButtonElement).disabled).toBe(true);
	});

	it("disables offline /switch inspection when history is not already cached", () => {
		render(
			<SlashMenu
				commands={COMMANDS}
				visible
				selectedIndex={0}
				mutationBlockedReason={WAITING_FOR_CONNECTION}
				onSetIndex={() => undefined}
				onSelect={() => undefined}
			/>,
		);

		expect((screen.getByRole("option", { name: /switch/i }) as HTMLButtonElement).disabled).toBe(true);
	});

	it("keeps an unsent new-session task editable offline", async () => {
		const user = userEvent.setup();
		render(
			<Composer
				selectedId={null}
				selectedIsSubagent={false}
				composerHandleRef={createRef<ComposerHandle>()}
				sending={false}
				canStop={false}
				stopping={false}
				queuedInputs={[]}
				mutationBlockedReason={WAITING_FOR_CONNECTION}
				onSubmit={() => true}
				onStop={() => undefined}
				onPromoteQueued={() => undefined}
				onUpdateQueued={() => undefined}
				onCancelQueued={() => undefined}
				onMoveQueued={() => undefined}
			/>,
		);

		await user.type(screen.getByRole("textbox"), "configure this task offline");
		expect((screen.getByRole("textbox") as HTMLTextAreaElement).value).toBe("configure this task offline");
		expect((screen.getByRole("button", { name: "send message" }) as HTMLButtonElement).disabled).toBe(true);
	});
});

describe("representative connection gates", () => {
	it("gates queue mutations but leaves queued draft editing available", async () => {
		const input: QueuedInput = {
			input_id: "input-1",
			priority: "follow_up",
			status: "queued",
			content: [{ type: "text", text: "queued draft" }],
			content_type: "user_message",
			created_at: "2026-01-01T00:00:00Z",
			updated_at: "2026-01-01T00:00:00Z",
		};
		const user = userEvent.setup();
		render(
			<QueuedInputPane
				inputs={[input]}
				visible
				mutationBlockedReason={WAITING_FOR_CONNECTION}
				onPromote={() => undefined}
				onUpdate={() => undefined}
				onCancel={() => undefined}
				onMove={() => undefined}
			/>,
		);
		expect(screen.getByText(WAITING_FOR_CONNECTION)).toBeTruthy();
		expect((screen.getByRole("button", { name: "promote to steer" }) as HTMLButtonElement).disabled).toBe(true);
		expect((screen.getByRole("button", { name: "delete queued follow-up" }) as HTMLButtonElement).disabled).toBe(true);
		await user.click(screen.getByRole("button", { name: "edit queued follow-up" }));
		expect(screen.getByDisplayValue("queued draft")).toBeTruthy();
		expect((screen.getByRole("button", { name: "save queued message" }) as HTMLButtonElement).disabled).toBe(true);
	});

	it("applies the central reason to the archive menu while leaving Rename dialog opening local", () => {
		const items = sessionMenuItems({
			archived: false,
			canArchive: true,
			canDelete: true,
			onRename: () => undefined,
			onArchiveToggle: () => undefined,
			onDelete: () => undefined,
			mutationBlockedReason: WAITING_FOR_CONNECTION,
		});
		expect(items.find((item) => item.id === "rename")?.disabled).not.toBe(true);
		expect(items.find((item) => item.id === "archive")).toMatchObject({
			disabled: true,
			disabledReason: WAITING_FOR_CONNECTION,
		});
	});

	it.each(["rename", "delete", "project"] as const)("keeps the %s dialog editable/open but disables server submit visibly", (kind) => {
		const common = { mutationBlockedReason: WAITING_FOR_CONNECTION };
		if (kind === "rename") {
			render(<RenameSessionDialog value="Draft name" onChange={() => undefined} onClose={() => undefined} onSubmit={() => undefined} {...common} />);
			expect((screen.getByRole("textbox", { name: "Session title" }) as HTMLInputElement).disabled).toBe(false);
			expect((screen.getByRole("button", { name: "Save" }) as HTMLButtonElement).disabled).toBe(true);
		} else if (kind === "delete") {
			render(<DeleteSessionDialog session={session} deleting={false} onClose={() => undefined} onConfirm={() => undefined} {...common} />);
			expect((screen.getByRole("button", { name: "Delete" }) as HTMLButtonElement).disabled).toBe(true);
		} else {
			render(<ProjectDialog state={projectState} onChange={() => undefined} onClose={() => undefined} onSubmit={() => undefined} {...common} />);
			expect((screen.getByRole("textbox", { name: "Project name" }) as HTMLInputElement).disabled).toBe(false);
			expect((screen.getByRole("button", { name: "Save" }) as HTMLButtonElement).disabled).toBe(true);
		}
		expect(screen.getByText(WAITING_FOR_CONNECTION)).toBeTruthy();
	});

	it("gates model/reasoning header controls with a focusable reason", () => {
		render(
			<LogHeader
				archived={false}
				status="idle"
				title="Cached session"
				modelOptions={[{ id: "gpt-test", label: "GPT test" }]}
				modelValue="gpt-test"
				modelDisabled
				modelDisabledTitle={WAITING_FOR_CONNECTION}
				reasoningDisabled
				controlsBlockedReason={WAITING_FOR_CONNECTION}
				reasoningEfforts={["medium"]}
				reasoningEffort="medium"
				onModelChange={() => undefined}
				onReasoningEffortChange={() => undefined}
				rightOpen
				onToggleRight={() => undefined}
			/>,
		);
		expect((screen.getByRole("combobox", { name: "Model" }) as HTMLSelectElement).disabled).toBe(true);
		expect((screen.getByRole("combobox", { name: "Reasoning effort" }) as HTMLSelectElement).disabled).toBe(true);
		expect(screen.getByText(WAITING_FOR_CONNECTION).getAttribute("tabindex")).toBe("0");
	});

	it("keeps cached session navigation enabled", async () => {
		const onSelect = vi.fn();
		const user = userEvent.setup();
		render(
			<ul>
				<SessionRow
					session={session}
					selected={false}
					onSelect={onSelect}
					onRename={() => undefined}
					onArchiveToggle={() => undefined}
					onDelete={() => undefined}
					mutationBlockedReason={WAITING_FOR_CONNECTION}
				/>
			</ul>,
		);

		const cachedSession = screen.getAllByRole("button", { name: /Cached session/ })
			.find((button) => !button.hasAttribute("aria-haspopup")) as HTMLButtonElement;
		expect(cachedSession.disabled).toBe(false);
		await user.click(cachedSession);
		expect(onSelect).toHaveBeenCalledTimes(1);
	});

	it("re-enables an open dialog without changing its value or focus", () => {
		const { rerender } = render(
			<RenameSessionDialog
				value="Offline title draft"
				onChange={() => undefined}
				onClose={() => undefined}
				onSubmit={() => undefined}
				mutationBlockedReason={WAITING_FOR_CONNECTION}
			/>,
		);
		const input = screen.getByRole("textbox", { name: "Session title" }) as HTMLInputElement;
		input.focus();

		rerender(
			<RenameSessionDialog
				value="Offline title draft"
				onChange={() => undefined}
				onClose={() => undefined}
				onSubmit={() => undefined}
				mutationBlockedReason={null}
			/>,
		);

		expect(input.value).toBe("Offline title draft");
		expect(document.activeElement).toBe(input);
		expect((screen.getByRole("button", { name: "Save" }) as HTMLButtonElement).disabled).toBe(false);
	});

	it("gates delegation Cancel/Re-run and history switching while local navigation remains present", () => {
		render(
			<RunBoardDelegationList
				delegations={[delegation]}
				showAllDelegations
				onToggleShowAllDelegations={() => undefined}
				onCancelDelegation={() => undefined}
				onReRunDelegation={() => undefined}
				mutationBlockedReason={WAITING_FOR_CONNECTION}
			/>,
		);
		expect((screen.getByRole("button", { name: /cancel/i }) as HTMLButtonElement).disabled).toBe(true);
		cleanup();

		render(
			<CompactHistoryPickerDialog
				nodes={[historyNode]}
				activeLeafId={null}
				onClose={() => undefined}
				onSwitch={() => undefined}
				mutationBlockedReason={WAITING_FOR_CONNECTION}
			/>,
		);
		expect((screen.getByRole("treeitem") as HTMLButtonElement).disabled).toBe(true);
		expect(screen.getByRole("button", { name: "close picker" })).toBeTruthy();
	});

	it("gates an uncached delegation page but keeps locally cached expansion available", () => {
		const { rerender } = render(
			<RunBoardDelegationList
				delegations={[delegation]}
				hasMoreDelegations
				showAllDelegations={false}
				onToggleShowAllDelegations={() => undefined}
				onCancelDelegation={() => undefined}
				onReRunDelegation={() => undefined}
				remoteReadBlockedReason={WAITING_FOR_CONNECTION}
			/>,
		);
		expect((screen.getByRole("button", { name: "see more" }) as HTMLButtonElement).disabled).toBe(true);
		expect(screen.getByText(WAITING_FOR_CONNECTION).getAttribute("tabindex")).toBe("0");

		rerender(
			<RunBoardDelegationList
				delegations={[delegation]}
				hasMoreDelegations
				showAllDelegations={false}
				expandedDelegationsAvailable
				onToggleShowAllDelegations={() => undefined}
				onCancelDelegation={() => undefined}
				onReRunDelegation={() => undefined}
				remoteReadBlockedReason={WAITING_FOR_CONNECTION}
			/>,
		);
		expect((screen.getByRole("button", { name: "see more" }) as HTMLButtonElement).disabled).toBe(false);
	});

	it("gates terminal resume with a visible focusable reason", () => {
		render(
			<MessageList
				entries={[terminalEntry]}
				activeLeafId="entry-1"
				isRunning={false}
				serverTimeMs={null}
				hasSession
				sessionId="session-1"
				entriesSessionId="session-1"
				onResumeTurn={() => undefined}
				resumeBlockedReason={WAITING_FOR_CONNECTION}
			/>,
		);

		expect((screen.getByRole("button", { name: "Continue" }) as HTMLButtonElement).disabled).toBe(true);
		expect(screen.getByText(WAITING_FOR_CONNECTION).getAttribute("tabindex")).toBe("0");
	});
});

const session: SessionSummary = {
	session_id: "session-1",
	project_id: null,
	outer_cwd: "/workspace",
	workspaces: [],
	activity: "idle",
	active_leaf_id: null,
	provider: { kind: "openai", model: "gpt-test" },
	metadata: { title: "Cached session" },
	created_at: "2026-01-01T00:00:00Z",
	updated_at: "2026-01-01T00:00:00Z",
};

const projectState: ProjectDialogState = {
	mode: "create",
	name: "Offline draft",
	workspaces: [{
		kind: "git",
		workspace_dir: "pi-relay",
		remote_url: "https://example.test/pi-relay.git",
		remote_branch: "main",
	}],
	saving: false,
};

const delegation: Delegation = {
	delegation_id: "delegation-1",
	kind: "full",
	status: "running",
	workflow: null,
	label: "work",
	progress: { expected: 1, spawned: 1, terminal: 0, running: 1, failed: 0 },
	handoff_dir: "/tmp/delegation-1",
	subagents: [{
		id: "child-1",
		status: "running",
		activity: "running",
		role: "implementer",
		subagent_type: "full",
		task_prompt_file: "child-1/task_prompt.md",
		final_message_file: null,
		transcript_file: null,
		outcome: null,
	}],
};

const historyNode: TranscriptTreeNode = {
	id: "entry-1",
	parent_id: null,
	timestamp_ms: 1,
	sequence: 1,
	item_type: "turn_finished",
	turn_id: 1,
	outcome: "Graceful",
	can_switch_to: true,
	edit_target_leaf_id: null,
	display_hint: "cached turn",
};

const terminalEntry = {
	id: "entry-1",
	parent_id: null,
	timestamp_ms: 1,
	sequence: 1,
	item: {
		type: "turn_finished" as const,
		turn_id: 1,
		outcome: "Interrupted" as const,
	},
};

function deferred<T>() {
	let resolve!: (value: T | PromiseLike<T>) => void;
	let reject!: (reason?: unknown) => void;
	const promise = new Promise<T>((resolvePromise, rejectPromise) => {
		resolve = resolvePromise;
		reject = rejectPromise;
	});
	return { promise, resolve, reject };
}
