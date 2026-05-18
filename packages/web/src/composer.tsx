import { memo, useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState, type KeyboardEvent, type RefObject } from "react";
import { Loader2, MoveUp, Send, Square } from "lucide-react";
import { COMMANDS, filterCommands, matchSlashPrefix, type SlashCommandInfo } from "./slash.ts";
import { contentBlocksToText, firstLine, truncate } from "./text.ts";
import type { QueuePaneInput } from "./pendingInputs.ts";

const NEW_SESSION_DRAFT_ID = "__new_session__";
const COMPOSER_DRAFTS_STORAGE_KEY = "piRelayComposerDrafts:v1";
const COMPOSER_MIN_HEIGHT_PX = 44;
const COMPOSER_MAX_HEIGHT_PX = 180;

type ComposerSubmitShortcutEvent = Pick<KeyboardEvent<HTMLTextAreaElement>, "ctrlKey" | "key" | "metaKey">;

export function isComposerSubmitShortcut(event: ComposerSubmitShortcutEvent): boolean {
	return event.key === "Enter" && (event.metaKey || event.ctrlKey);
}

export interface ComposerHandle {
	focus(): void;
	getValue(): string;
	setValue(value: string): void;
	setValueForSession(sessionId: string | null, value: string): void;
	clearSession(sessionId: string | null): void;
	restoreSubmittedDraft(sessionId: string | null, value: string): void;
}

export type ComposerDraftStorage = Pick<Storage, "getItem" | "setItem" | "removeItem">;

export const Composer = memo(function Composer({
	selectedId,
	hasProject,
	composerHandleRef,
	sending,
	canStop,
	stopping,
	queuedInputs,
	onSubmit,
	onStop,
	onPromoteQueued
}: {
	selectedId: string | null;
	hasProject: boolean;
	composerHandleRef: RefObject<ComposerHandle | null>;
	sending: boolean;
	canStop: boolean;
	stopping: boolean;
	queuedInputs: QueuePaneInput[];
	onSubmit: (text: string) => Promise<boolean> | boolean;
	onStop: () => void;
	onPromoteQueued: (inputId: string) => void;
}) {
	const textAreaRef = useRef<HTMLTextAreaElement | null>(null);
	const selectedIdRef = useRef<string | null>(selectedId);
	const draftsRef = useRef(loadComposerDrafts());
	const draftRef = useRef("");
	const initialDraft = draftsRef.current.get(composerDraftKey(selectedId)) ?? "";
	const [draft, setDraft] = useState(initialDraft);
	const [slashIndex, setSlashIndex] = useState(0);

	useEffect(() => {
		draftRef.current = initialDraft;
	}, [initialDraft]);

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
			if (value.trim()) draftsRef.current.set(key, value);
			else draftsRef.current.delete(key);
			saveComposerDrafts(draftsRef.current);
		},
		[]
	);

	const setDraftValue = useCallback(
		(value: string) => {
			draftRef.current = value;
			setDraft(value);
			storeDraft(selectedIdRef.current, value);
		},
		[storeDraft]
	);

	useEffect(() => {
		selectedIdRef.current = selectedId;
		const nextDraft = draftsRef.current.get(composerDraftKey(selectedId)) ?? "";
		draftRef.current = nextDraft;
		setDraft(nextDraft);
	}, [selectedId]);

	useEffect(() => {
		composerHandleRef.current = {
			focus: () => textAreaRef.current?.focus(),
			getValue: () => draftRef.current,
			setValue: (value) => setDraftValue(value),
			setValueForSession: (sessionId, value) => {
				storeDraft(sessionId, value);
				if (selectedIdRef.current === sessionId) {
					draftRef.current = value;
					setDraft(value);
				}
			},
			restoreSubmittedDraft: (sessionId, value) => {
				storeDraft(sessionId, value);
				if (selectedIdRef.current === sessionId && !draftRef.current.trim()) {
					draftRef.current = value;
					setDraft(value);
				}
			},
			clearSession: (sessionId) => {
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
	}, [composerHandleRef, setDraftValue, storeDraft]);

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
		if (!text || sending) return;
		const submittedSessionId = selectedIdRef.current;
		storeDraft(submittedSessionId, "");
		setDraftValue("");
		requestAnimationFrame(() => textAreaRef.current?.focus());
		const accepted = await onSubmit(text);
		if (!accepted && !draftRef.current.trim()) setDraftValue(text);
	}, [onSubmit, sending, setDraftValue, storeDraft]);

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
				onSetIndex={setSlashIndex}
				onSelect={(command) => setDraftValue(`/${command.name} `)}
			/>
			<QueuedInputPane
				inputs={queuedInputs}
				visible={queuedInputs.length > 0 && !slashState.visible}
				onPromote={onPromoteQueued}
			/>
			<textarea
				ref={textAreaRef}
				value={draft}
				onChange={(event) => setDraftValue(event.target.value)}
				onKeyDown={onKeyDown}
				placeholder={selectedId ? "Message the session or type /" : hasProject ? "Create or select a session" : "Select a project first"}
				className="composer"
				rows={1}
				enterKeyHint="enter"
				title="Enter for newline. Cmd+Enter to send."
			/>
			<button
				className="stop-button"
				type="button"
				onClick={onStop}
				disabled={!canStop || stopping}
				title="stop active turn"
				aria-label="stop active turn"
			>
				{stopping ? <Loader2 className="spin" size={15} /> : <Square size={14} />}
			</button>
			<button
				className="send-button"
				type="button"
				onClick={() => void sendDraft()}
				disabled={sending || !draft.trim()}
				title="send (Cmd+Enter)"
				aria-label="send message"
			>
				{sending ? <Loader2 className="spin" size={16} /> : <Send size={16} />}
			</button>
		</div>
	);
});

export function QueuedInputPane({
	inputs,
	visible,
	onPromote
}: {
	inputs: QueuePaneInput[];
	visible: boolean;
	onPromote: (inputId: string) => void;
}) {
	if (!visible) return null;
	return (
		<div className="queue-pane">
			<div className="queue-pane-head">
				<span>Queued messages</span>
				<code>{inputs.length}</code>
			</div>
			<div className="queue-list">
				{inputs.map((input) => {
					const canPromote = !input.pending && input.priority === "follow_up" && input.status === "queued";
					return (
						<div className={`queue-row ${input.pending ? "pending" : ""}`} key={input.input_id}>
							<span className="queue-preview">{truncate(firstLine(contentBlocksToText(input.content)) || "(empty)", 96)}</span>
							<button
								className="queue-steer-button"
								type="button"
								onClick={() => onPromote(input.input_id)}
								disabled={!canPromote}
								title={canPromote ? "promote to steer" : "already steering"}
							>
								<MoveUp size={13} />
								<span>{input.pending ? "sending" : input.priority === "steer" ? "steering" : "steer"}</span>
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
	onSetIndex,
	onSelect
}: {
	commands: typeof COMMANDS;
	visible: boolean;
	selectedIndex: number;
	onSetIndex: (index: number) => void;
	onSelect: (command: SlashCommandInfo) => void;
}) {
	if (!visible || commands.length === 0) return null;
	return (
		<div className="slash-menu" role="listbox" aria-label="slash commands">
			{commands.map((command, index) => (
				<button
					type="button"
					key={command.name}
					className={`slash-row ${index === selectedIndex ? "selected" : ""}`}
					role="option"
					aria-selected={index === selectedIndex}
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
				</button>
			))}
		</div>
	);
}
