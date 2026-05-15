import { memo, useCallback, useEffect, useMemo, useRef, useState, type KeyboardEvent, type RefObject } from "react";
import { Loader2, MoveUp, Send, Square } from "lucide-react";
import { COMMANDS, filterCommands, matchSlashPrefix, type SlashCommandInfo } from "./slash.ts";
import { contentBlocksToText, firstLine, truncate } from "./text.ts";
import type { QueuedInput } from "./types.ts";

const NEW_SESSION_DRAFT_ID = "__new_session__";

export interface ComposerHandle {
	focus(): void;
	getValue(): string;
	setValue(value: string): void;
	setValueForSession(sessionId: string | null, value: string): void;
	clearSession(sessionId: string | null): void;
}

export const Composer = memo(function Composer({
	selectedId,
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
	composerHandleRef: RefObject<ComposerHandle | null>;
	sending: boolean;
	canStop: boolean;
	stopping: boolean;
	queuedInputs: QueuedInput[];
	onSubmit: (text: string) => Promise<boolean> | boolean;
	onStop: () => void;
	onPromoteQueued: (inputId: string) => void;
}) {
	const textAreaRef = useRef<HTMLTextAreaElement | null>(null);
	const selectedIdRef = useRef<string | null>(selectedId);
	const draftsRef = useRef(new Map<string, string>());
	const draftRef = useRef("");
	const [draft, setDraft] = useState("");
	const [slashIndex, setSlashIndex] = useState(0);

	const draftKey = useCallback((sessionId: string | null) => sessionId ?? NEW_SESSION_DRAFT_ID, []);

	const storeDraft = useCallback(
		(sessionId: string | null, value: string) => {
			const key = draftKey(sessionId);
			if (value.trim()) draftsRef.current.set(key, value);
			else draftsRef.current.delete(key);
		},
		[draftKey]
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
		const nextDraft = draftsRef.current.get(draftKey(selectedId)) ?? "";
		draftRef.current = nextDraft;
		setDraft(nextDraft);
	}, [draftKey, selectedId]);

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

	const sendDraft = useCallback(async () => {
		const text = draftRef.current.trim();
		if (!text || sending) return;
		const submittedSessionId = selectedIdRef.current;
		const accepted = await onSubmit(text);
		if (!accepted) return;
		storeDraft(submittedSessionId, "");
		setDraftValue("");
		requestAnimationFrame(() => textAreaRef.current?.focus());
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
				if (event.key === "Enter" && !event.shiftKey) {
					event.preventDefault();
					const command = slashState.commands[Math.min(slashIndex, slashState.commands.length - 1)];
					const typedCommand = matchSlashPrefix(draftRef.current) ?? "";
					if (command.name === typedCommand) {
						void sendDraft();
					} else {
						setDraftValue(`/${command.name} `);
					}
					return;
				}
			}
			if (event.key === "Enter" && !event.shiftKey) {
				event.preventDefault();
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
				placeholder={selectedId ? "Message the session or type /" : "Create or select a session"}
				className="composer"
				rows={1}
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
			<button className="send-button" type="button" onClick={() => void sendDraft()} disabled={sending || !draft.trim()}>
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
	inputs: QueuedInput[];
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
					const canPromote = input.priority === "follow_up" && input.status === "queued";
					return (
						<div className="queue-row" key={input.input_id}>
							<span className={`queue-priority ${input.priority === "steer" ? "steer" : ""}`}>
								{input.priority === "steer" ? "steer" : "follow-up"}
							</span>
							<span className="queue-preview">{truncate(firstLine(contentBlocksToText(input.content)) || "(empty)", 96)}</span>
							<button
								className="queue-steer-button"
								type="button"
								onClick={() => onPromote(input.input_id)}
								disabled={!canPromote}
								title={canPromote ? "promote to steer" : "already steering"}
							>
								<MoveUp size={13} />
								<span>{input.priority === "steer" ? "steering" : "steer"}</span>
							</button>
						</div>
					);
				})}
			</div>
		</div>
	);
}

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
