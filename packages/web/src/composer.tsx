import { memo, useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState, type KeyboardEvent, type RefObject } from "react";
import { ArrowDown, ArrowUp, Check, Edit3, Loader2, Send, ShipWheel, Square, Trash2, X } from "lucide-react";
import type { ComposerSubmission } from "./composerRouting.ts";
import { composerTextNeedsConnection, ConnectionBlockedReason } from "./connectionRecovery.tsx";
import { randomId } from "./ids.ts";
import { COMMANDS, filterCommands, matchSlashPrefix, type SlashCommandInfo } from "./slash.ts";
import { contentBlocksToText, firstLine, truncate } from "./text.ts";
import type { QueuedInput } from "./types.ts";

const NEW_SESSION_DRAFT_ID = "__new_session__";
const COMPOSER_DRAFTS_STORAGE_KEY = "piRelayComposerDrafts:v1";
const COMPOSER_MIN_HEIGHT_PX = 44;
const COMPOSER_MAX_HEIGHT_PX = 180;

type ComposerSubmitShortcutEvent = Pick<KeyboardEvent<HTMLTextAreaElement>, "ctrlKey" | "key" | "metaKey">;
export type PendingSubmittedDraft = {
	value: string;
	version: number;
	newSessionSetupGeneration: number;
	clientControlId: string;
	newSessionId: string;
};
export type SubmittedDraftResolution = "ignore" | "apply";

export function submittedDraftStillCurrent(
	pending: PendingSubmittedDraft | undefined,
	currentVersion: number | undefined,
	value: string,
	version?: number,
): boolean {
	if (!pending || pending.value !== value) return false;
	const expectedVersion = version ?? pending.version;
	return pending.version === expectedVersion && currentVersion === expectedVersion;
}

export function resolveSubmittedDraft(
	pending: PendingSubmittedDraft | undefined,
	currentVersion: number | undefined,
	value: string,
	version?: number,
): SubmittedDraftResolution {
	return submittedDraftStillCurrent(pending, currentVersion, value, version) ? "apply" : "ignore";
}

export function isComposerSubmitShortcut(event: ComposerSubmitShortcutEvent): boolean {
	return event.key === "Enter" && (event.metaKey || event.ctrlKey);
}

export function submissionIdsForDraft(
	pending: PendingSubmittedDraft | undefined,
	text: string,
	newSessionSetupGeneration: number,
	createId: (prefix: string) => string = randomId,
): Pick<PendingSubmittedDraft, "clientControlId" | "newSessionId"> {
	if (
		pending?.value === text &&
		pending.newSessionSetupGeneration === newSessionSetupGeneration
	) {
		return {
			clientControlId: pending.clientControlId,
			newSessionId: pending.newSessionId,
		};
	}
	return {
		clientControlId: createId("web_control"),
		newSessionId: createId("session"),
	};
}

export interface ComposerHandle {
	focus(): void;
	focusTarget(): HTMLElement | null;
	getValue(): string;
	setValue(value: string): void;
	setSessionDraft(sessionId: string | null, value: string): void;
	clearSession(sessionId: string | null): void;
	restoreSubmittedDraft(sessionId: string | null, value: string): void;
}

export type ComposerDraftStorage = Pick<Storage, "getItem" | "setItem" | "removeItem">;

export const Composer = memo(function Composer({
	selectedId,
	selectedIsSubagent,
	composerHandleRef,
	sending,
	canStop,
	stopping,
	queuedInputs,
	mutationBlockedReason,
	newSessionSetupGeneration = 0,
	onSubmit,
	onStop,
	onPromoteQueued,
	onUpdateQueued,
	onCancelQueued,
	onMoveQueued,
}: {
	selectedId: string | null;
	selectedIsSubagent: boolean;
	composerHandleRef: RefObject<ComposerHandle | null>;
	sending: boolean;
	canStop: boolean;
	stopping: boolean;
	queuedInputs: QueuedInput[];
	mutationBlockedReason?: string | null;
	newSessionSetupGeneration?: number;
	onSubmit: (submission: ComposerSubmission) => Promise<boolean> | boolean;
	onStop: () => void;
	onPromoteQueued: (inputId: string) => void;
	onUpdateQueued: (inputId: string, text: string) => void;
	onCancelQueued: (inputId: string) => void;
	onMoveQueued: (inputId: string, direction: "up" | "down") => void;
}) {
	const textAreaRef = useRef<HTMLTextAreaElement | null>(null);
	const selectedIdRef = useRef<string | null>(selectedId);
	const draftsRef = useRef(loadComposerDrafts());
	const draftVersionsRef = useRef(new Map<string, number>());
	const pendingSubmittedDraftsRef = useRef(new Map<string, PendingSubmittedDraft>());
	const initialDraft = draftsRef.current.get(composerDraftKey(selectedId)) ?? "";
	const draftRef = useRef(initialDraft);
	const [draft, setDraft] = useState(initialDraft);
	const [slashIndex, setSlashIndex] = useState(0);

	const resizeComposer = useCallback(() => {
		const textArea = textAreaRef.current;
		if (!textArea) return;
		textArea.style.height = "auto";
		const nextHeight = Math.min(
			Math.max(textArea.scrollHeight, COMPOSER_MIN_HEIGHT_PX),
			COMPOSER_MAX_HEIGHT_PX
		);
		textArea.style.height = `${nextHeight}px`;
		textArea.style.overflowY = textArea.scrollHeight > COMPOSER_MAX_HEIGHT_PX ? "auto" : "hidden";
	}, []);

	const storeDraft = useCallback(
		(sessionId: string | null, value: string) => {
			const key = composerDraftKey(sessionId);
			const version = (draftVersionsRef.current.get(key) ?? 0) + 1;
			draftVersionsRef.current.set(key, version);
			if (value.trim()) draftsRef.current.set(key, value);
			else draftsRef.current.delete(key);
			saveComposerDrafts(draftsRef.current);
			return version;
		},
		[]
	);

	const clearSubmittedDraft = useCallback(
		(sessionId: string | null, value: string, version: number) => {
			const key = composerDraftKey(sessionId);
			const pending = pendingSubmittedDraftsRef.current.get(key);
			if (resolveSubmittedDraft(pending, draftVersionsRef.current.get(key), value, version) !== "apply") {
				if (pending?.value === value && pending.version === version) pendingSubmittedDraftsRef.current.delete(key);
				return;
			}
			pendingSubmittedDraftsRef.current.delete(key);
			if (draftsRef.current.get(key) === value) {
				storeDraft(sessionId, "");
				if (selectedIdRef.current === sessionId && draftRef.current === value) {
					draftRef.current = "";
					setDraft("");
				}
			}
		},
		[storeDraft]
	);

	const setSessionDraft = useCallback(
		(sessionId: string | null, value: string) => {
			pendingSubmittedDraftsRef.current.delete(composerDraftKey(sessionId));
			storeDraft(sessionId, value);
			if (selectedIdRef.current === sessionId) {
				draftRef.current = value;
				setDraft(value);
			}
		},
		[storeDraft],
	);

	const restoreSubmittedDraft = useCallback(
		(sessionId: string | null, value: string, version?: number) => {
			const key = composerDraftKey(sessionId);
			const pending = pendingSubmittedDraftsRef.current.get(key);
			if (resolveSubmittedDraft(pending, draftVersionsRef.current.get(key), value, version) !== "apply") {
				if (pending?.value === value && (version === undefined || pending.version === version)) {
					pendingSubmittedDraftsRef.current.delete(key);
				}
				return;
			}
			const restoredVersion = storeDraft(sessionId, value);
			pendingSubmittedDraftsRef.current.set(key, {
				...pending!,
				version: restoredVersion,
			});
			if (selectedIdRef.current === sessionId && !draftRef.current.trim()) {
				draftRef.current = value;
				setDraft(value);
			}
		},
		[storeDraft]
	);

	const setDraftValue = useCallback(
		(value: string) => {
			const key = composerDraftKey(selectedIdRef.current);
			const pending = pendingSubmittedDraftsRef.current.get(key);
			if (pending && pending.value !== value.trim()) {
				pendingSubmittedDraftsRef.current.delete(key);
			}
			draftRef.current = value;
			setDraft(value);
			storeDraft(selectedIdRef.current, value);
		},
		[storeDraft]
	);

	useLayoutEffect(() => {
		selectedIdRef.current = selectedId;
		const nextDraft = draftsRef.current.get(composerDraftKey(selectedId)) ?? "";
		draftRef.current = nextDraft;
		setDraft(nextDraft);
	}, [selectedId]);

	useEffect(() => {
		composerHandleRef.current = {
			focus: () => textAreaRef.current?.focus(),
			focusTarget: () => textAreaRef.current,
			getValue: () => draftRef.current,
			setValue: (value) => setDraftValue(value),
			setSessionDraft: (sessionId, value) => setSessionDraft(sessionId, value),
			restoreSubmittedDraft: (sessionId, value) => restoreSubmittedDraft(sessionId, value),
			clearSession: (sessionId) => {
				pendingSubmittedDraftsRef.current.delete(composerDraftKey(sessionId));
				storeDraft(sessionId, "");
				if (selectedIdRef.current === sessionId) {
					draftRef.current = "";
					setDraft("");
				}
			}
		};
		return () => {
			if (composerHandleRef.current?.getValue() === draftRef.current) {
				composerHandleRef.current = null;
			}
		};
	}, [composerHandleRef, restoreSubmittedDraft, setDraftValue, setSessionDraft, storeDraft]);

	const slashState = useMemo<{ visible: boolean; commands: typeof COMMANDS }>(() => {
		const prefix = matchSlashPrefix(draft);
		if (prefix === null) return { visible: false, commands: [] };
		return { visible: true, commands: filterCommands(prefix) };
	}, [draft]);

	useEffect(() => {
		setSlashIndex(0);
	}, [slashState.commands, slashState.visible]);

	useLayoutEffect(() => {
		resizeComposer();
	}, [draft, resizeComposer]);

	useEffect(() => {
		const textArea = textAreaRef.current;
		const target = textArea?.parentElement;
		if (!target || typeof ResizeObserver === "undefined") return;
		const observer = new ResizeObserver(() => resizeComposer());
		observer.observe(target);
		return () => observer.disconnect();
	}, [resizeComposer]);

	const sendDraft = useCallback(async () => {
		const text = draftRef.current.trim();
		if (
			!text ||
			sending ||
			(mutationBlockedReason && composerTextNeedsConnection(text))
		) {
			return;
		}
		const submittedSessionId = selectedIdRef.current;
		const key = composerDraftKey(submittedSessionId);
		const previous = pendingSubmittedDraftsRef.current.get(key);
		const submittedSetupGeneration =
			submittedSessionId === null ? newSessionSetupGeneration : 0;
		const { clientControlId, newSessionId } = submissionIdsForDraft(
			previous,
			text,
			submittedSetupGeneration,
		);
		const submittedVersion = storeDraft(submittedSessionId, text);
		pendingSubmittedDraftsRef.current.set(key, {
			value: text,
			version: submittedVersion,
			newSessionSetupGeneration: submittedSetupGeneration,
			clientControlId,
			newSessionId,
		});
		draftRef.current = "";
		setDraft("");
		requestAnimationFrame(() => {
			// A slash command can mount a modal before this callback runs.
			// Never pull focus back behind that modal.
			if (!document.querySelector('[role="dialog"], [role="alertdialog"]')) textAreaRef.current?.focus();
		});
		const accepted = await onSubmit({
			sessionId: submittedSessionId,
			text,
			clientControlId,
			newSessionId,
		});
		if (accepted) {
			clearSubmittedDraft(submittedSessionId, text, submittedVersion);
		} else {
			restoreSubmittedDraft(submittedSessionId, text, submittedVersion);
		}
	}, [clearSubmittedDraft, mutationBlockedReason, newSessionSetupGeneration, onSubmit, restoreSubmittedDraft, sending, storeDraft]);

	const onKeyDown = useCallback(
		(event: KeyboardEvent<HTMLTextAreaElement>) => {
			if (slashState.visible && slashState.commands.length > 0) {
				if (event.key === "ArrowDown") {
					event.preventDefault();
					setSlashIndex((index) => (index + 1) % slashState.commands.length);
					return;
				}
				if (event.key === "ArrowUp") {
					event.preventDefault();
					setSlashIndex((index) => (index - 1 + slashState.commands.length) % slashState.commands.length);
					return;
				}
				if (event.key === "Tab") {
					event.preventDefault();
					const command = slashState.commands[Math.min(slashIndex, slashState.commands.length - 1)];
					setDraftValue(`/${command.name} `);
					return;
				}
			}
			if (isComposerSubmitShortcut(event)) {
				event.preventDefault();
				if (slashState.visible && slashState.commands.length > 0) {
					const command = slashState.commands[Math.min(slashIndex, slashState.commands.length - 1)];
					const typedCommand = matchSlashPrefix(draftRef.current) ?? "";
					if (command.name !== typedCommand) {
						setDraftValue(`/${command.name} `);
						return;
					}
				}
				void sendDraft();
			}
		},
		[sendDraft, setDraftValue, slashIndex, slashState.commands, slashState.visible]
	);

	return (
		<div className="composer-wrap">
			<SlashMenu
				commands={slashState.commands}
				visible={slashState.visible}
				selectedIndex={slashIndex}
				mutationBlockedReason={mutationBlockedReason}
				onSetIndex={setSlashIndex}
				onSelect={(command) => setDraftValue(`/${command.name} `)}
			/>
			<QueuedInputPane
				inputs={queuedInputs}
				visible={queuedInputs.length > 0 && !slashState.visible}
				mutationBlockedReason={mutationBlockedReason}
				onPromote={onPromoteQueued}
				onUpdate={onUpdateQueued}
				onCancel={onCancelQueued}
				onMove={onMoveQueued}
			/>
			<textarea
				ref={textAreaRef}
				value={draft}
				onChange={(event) => setDraftValue(event.target.value)}
				onKeyDown={onKeyDown}
				placeholder={
					selectedIsSubagent
						? "Steer this subagent or type /"
						: selectedId
							? "Follow up with this session or type /"
							: "Create or select a session"
				}
				className="composer"
				rows={1}
				enterKeyHint="enter"
				title="Enter for newline. Cmd+Enter to send."
			/>
			<button
				className="stop-button"
				type="button"
				onClick={onStop}
				disabled={!canStop || stopping || !!mutationBlockedReason}
				aria-busy={stopping}
				title="stop active turn"
				aria-label="stop active turn"
			>
				{stopping ? <Loader2 className="spin" size={15} /> : <Square size={14} />}
			</button>
			<button
				className="send-button"
				type="button"
				onClick={() => void sendDraft()}
				disabled={
					sending ||
					!draft.trim() ||
					(!!mutationBlockedReason && composerTextNeedsConnection(draft))
				}
				aria-busy={sending}
				title="send (Cmd+Enter)"
				aria-label="send message"
			>
				{sending ? <Loader2 className="spin" size={16} /> : <Send size={16} />}
			</button>
			<ConnectionBlockedReason reason={mutationBlockedReason} className="composer-blocked-reason" />
		</div>
	);
});

export function QueuedInputPane({
	inputs,
	visible,
	mutationBlockedReason,
	onPromote,
	onUpdate,
	onCancel,
	onMove,
}: {
	inputs: QueuedInput[];
	visible: boolean;
	mutationBlockedReason?: string | null;
	onPromote: (inputId: string) => void;
	onUpdate: (inputId: string, text: string) => void;
	onCancel: (inputId: string) => void;
	onMove: (inputId: string, direction: "up" | "down") => void;
}) {
	const [editingId, setEditingId] = useState<string | null>(null);
	const [editingText, setEditingText] = useState("");
	const followUpIds = useMemo(
		() =>
			inputs
				.filter((input) => input.priority === "follow_up" && input.status === "queued")
				.map((input) => input.input_id),
		[inputs],
	);
	useEffect(() => {
		if (editingId && !inputs.some((input) => input.input_id === editingId)) {
			setEditingId(null);
			setEditingText("");
		}
	}, [editingId, inputs]);
	if (!visible) return null;
	return (
		<div className="queue-pane">
			<div className="queue-pane-head">
				<span>Queued messages</span>
				<code>{inputs.length}</code>
			</div>
			<ConnectionBlockedReason reason={mutationBlockedReason} className="queue-blocked-reason" />
			<div className="queue-list">
				{inputs.map((input) => {
					const canPromote = input.priority === "follow_up" && input.status === "queued";
					const canMutate = canPromote;
					const followUpIndex = followUpIds.indexOf(input.input_id);
					const isEditing = editingId === input.input_id;
					const preview = contentBlocksToText(input.content);
					return (
						<div className="queue-row" key={input.input_id}>
							{isEditing ? (
								<textarea
									className="queue-edit"
									value={editingText}
									onChange={(event) => setEditingText(event.target.value)}
									rows={Math.min(4, Math.max(2, editingText.split("\n").length))}
									autoFocus
								/>
							) : (
								<span className="queue-preview">{truncate(firstLine(preview) || "(empty)", 96)}</span>
							)}
							{isEditing ? (
								<div className="queue-actions">
									<button
										className="queue-icon-button"
										type="button"
										onClick={() => {
											const nextText = editingText.trim();
											if (!nextText) return;
											onUpdate(input.input_id, nextText);
											setEditingId(null);
										}}
										disabled={!editingText.trim() || !!mutationBlockedReason}
										title="save queued message"
										aria-label="save queued message"
									>
										<Check size={13} />
									</button>
									<button
										className="queue-icon-button"
										type="button"
										onClick={() => {
											setEditingId(null);
											setEditingText("");
										}}
										title="cancel edit"
										aria-label="cancel edit"
									>
										<X size={13} />
									</button>
								</div>
							) : (
								<div className="queue-actions">
									<button
										className="queue-icon-button"
										type="button"
										onClick={() => onMove(input.input_id, "up")}
										disabled={!canMutate || followUpIndex <= 0 || !!mutationBlockedReason}
										title="move queued follow-up up"
										aria-label="move queued follow-up up"
									>
										<ArrowUp size={13} />
									</button>
									<button
										className="queue-icon-button"
										type="button"
										onClick={() => onMove(input.input_id, "down")}
										disabled={!canMutate || followUpIndex < 0 || followUpIndex >= followUpIds.length - 1 || !!mutationBlockedReason}
										title="move queued follow-up down"
										aria-label="move queued follow-up down"
									>
										<ArrowDown size={13} />
									</button>
									<button
										className="queue-icon-button"
										type="button"
										onClick={() => {
											setEditingId(input.input_id);
											setEditingText(preview);
										}}
										disabled={!canMutate}
										title={canMutate ? "edit queued follow-up" : "steering messages cannot be edited"}
										aria-label="edit queued follow-up"
									>
										<Edit3 size={13} />
									</button>
									<button
										className="queue-icon-button destructive"
										type="button"
										onClick={() => onCancel(input.input_id)}
										disabled={!canMutate || !!mutationBlockedReason}
										title={canMutate ? "delete queued follow-up" : "steering messages cannot be deleted here"}
										aria-label="delete queued follow-up"
									>
										<Trash2 size={13} />
									</button>
								</div>
							)}
							<button
								className="queue-steer-button"
								type="button"
								onClick={() => onPromote(input.input_id)}
								disabled={!canPromote || !!mutationBlockedReason}
								title={canPromote ? "promote to steer" : "already steering"}
								aria-label={canPromote ? "promote to steer" : "already steering"}
							>
								<ShipWheel size={15} />
							</button>
						</div>
					);
				})}
			</div>
		</div>
	);
}

export function composerDraftKey(sessionId: string | null): string {
	return sessionId ?? NEW_SESSION_DRAFT_ID;
}

export function loadComposerDrafts(storage = browserStorage()): Map<string, string> {
	const drafts = new Map<string, string>();
	if (!storage) return drafts;
	try {
		const raw = storage.getItem(COMPOSER_DRAFTS_STORAGE_KEY);
		if (!raw) return drafts;
		const parsed = JSON.parse(raw) as unknown;
		if (!isRecord(parsed)) return drafts;
		const rawDrafts = parsed.drafts;
		if (!isRecord(rawDrafts)) return drafts;
		for (const [key, value] of Object.entries(rawDrafts)) {
			if (key && typeof value === "string" && value.trim()) drafts.set(key, value);
		}
	} catch {
		return new Map();
	}
	return drafts;
}

export function saveComposerDrafts(drafts: Map<string, string>, storage = browserStorage()): void {
	if (!storage) return;
	try {
		const entries = Array.from(drafts.entries()).filter(([key, value]) => key && value.trim());
		if (entries.length === 0) {
			storage.removeItem(COMPOSER_DRAFTS_STORAGE_KEY);
			return;
		}
		storage.setItem(
			COMPOSER_DRAFTS_STORAGE_KEY,
			JSON.stringify({
				drafts: Object.fromEntries(entries),
				updatedAt: Date.now(),
			}),
		);
	} catch {
		// localStorage can be unavailable or full; draft persistence is best-effort.
	}
}

function browserStorage(): ComposerDraftStorage | null {
	if (typeof window === "undefined") return null;
	try {
		return window.localStorage ?? null;
	} catch {
		return null;
	}
}

function isRecord(value: unknown): value is Record<string, unknown> {
	return typeof value === "object" && value !== null && !Array.isArray(value);
}

export { COMPOSER_DRAFTS_STORAGE_KEY };

export function SlashMenu({
	commands,
	visible,
	selectedIndex,
	mutationBlockedReason,
	onSetIndex,
	onSelect
}: {
	commands: typeof COMMANDS;
	visible: boolean;
	selectedIndex: number;
	mutationBlockedReason?: string | null;
	onSetIndex: (index: number) => void;
	onSelect: (command: SlashCommandInfo) => void;
}) {
	if (!visible || commands.length === 0) return null;
	return (
		<div className="slash-menu" role="listbox" aria-label="slash commands">
			{commands.map((command, index) => {
				const connectionRequired = composerTextNeedsConnection(`/${command.name}`);
				return (
				<button
					type="button"
					key={command.name}
					className={`slash-row ${index === selectedIndex ? "selected" : ""}`}
					role="option"
					aria-selected={index === selectedIndex}
					disabled={!!mutationBlockedReason && connectionRequired}
					onMouseEnter={() => onSetIndex(index)}
					onMouseDown={(event) => {
						event.preventDefault();
						onSelect(command);
					}}
				>
					<span className="slash-name">
						/{command.name}
						{command.argumentHint ? <small>{command.argumentHint}</small> : null}
					</span>
					<span className="slash-description">{command.description}</span>
					{mutationBlockedReason && connectionRequired ? (
						<span className="slash-disabled-reason">{mutationBlockedReason}</span>
					) : null}
				</button>
				);
			})}
		</div>
	);
}
