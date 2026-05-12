import type { KeyboardEvent, RefObject } from "react";
import { Loader2, MoveUp, Send, Square } from "lucide-react";
import { COMMANDS, type SlashCommandInfo } from "./slash.ts";
import { contentBlocksToText, firstLine, truncate } from "./text.ts";
import type { QueuedInput } from "./types.ts";

export function Composer({
	value,
	selectedId,
	textAreaRef,
	sending,
	canStop,
	stopping,
	slashCommands,
	slashVisible,
	slashIndex,
	queuedInputs,
	onChange,
	onKeyDown,
	onSend,
	onStop,
	onSetSlashIndex,
	onSelectSlash,
	onPromoteQueued
}: {
	value: string;
	selectedId: string | null;
	textAreaRef: RefObject<HTMLTextAreaElement | null>;
	sending: boolean;
	canStop: boolean;
	stopping: boolean;
	slashCommands: typeof COMMANDS;
	slashVisible: boolean;
	slashIndex: number;
	queuedInputs: QueuedInput[];
	onChange: (value: string) => void;
	onKeyDown: (event: KeyboardEvent<HTMLTextAreaElement>) => void;
	onSend: () => void;
	onStop: () => void;
	onSetSlashIndex: (index: number) => void;
	onSelectSlash: (command: SlashCommandInfo) => void;
	onPromoteQueued: (inputId: string) => void;
}) {
	return (
		<div className="composer-wrap">
			<SlashMenu
				commands={slashCommands}
				visible={slashVisible}
				selectedIndex={slashIndex}
				onSetIndex={onSetSlashIndex}
				onSelect={onSelectSlash}
			/>
			<QueuedInputPane
				inputs={queuedInputs}
				visible={queuedInputs.length > 0 && !slashVisible}
				onPromote={onPromoteQueued}
			/>
			<textarea
				ref={textAreaRef}
				value={value}
				onChange={(event) => onChange(event.target.value)}
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
			<button className="send-button" type="button" onClick={onSend} disabled={sending || !value.trim()}>
				{sending ? <Loader2 className="spin" size={16} /> : <Send size={16} />}
			</button>
		</div>
	);
}

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
