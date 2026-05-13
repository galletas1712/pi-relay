import { useEffect, useMemo, useRef, useState } from "react";
import { AlertTriangle, Check, ChevronDown, Copy, Loader2, Terminal, Wrench } from "lucide-react";
import rehypeRaw from "rehype-raw";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import { assistantMessageText } from "./exportTranscript.ts";
import { branchEntriesFor } from "./historyTargets.ts";
import { contentBlocksToText, firstLine } from "./text.ts";
import type { NoticeTone, TranscriptEntry, TranscriptItem } from "./types.ts";

type ToolResultItem = Extract<TranscriptItem, { type: "tool_result" }>;

export function MessageList({
	entries,
	activeLeafId,
	isRunning,
	hasSession
}: {
	entries: TranscriptEntry[];
	activeLeafId: string | null;
	isRunning: boolean;
	hasSession: boolean;
}) {
	const scrollRef = useRef<HTMLDivElement | null>(null);
	const visibleEntries = useMemo(
		() => (hasSession ? branchEntriesFor(entries, activeLeafId) : entries),
		[activeLeafId, entries, hasSession]
	);
	useEffect(() => {
		const node = scrollRef.current;
		if (!node) return;
		node.scrollTop = node.scrollHeight;
	}, [visibleEntries.length]);
	const toolIndex = useMemo(() => indexToolEntries(visibleEntries), [visibleEntries]);

	if (!hasSession) {
		return (
			<div className="message-scroll" ref={scrollRef}>
				<div className="empty-state">
					<Terminal size={34} />
					<span>Select or create a session</span>
				</div>
			</div>
		);
	}

	return (
		<div className="message-scroll" ref={scrollRef}>
			{visibleEntries.map((entry) => (
				<TranscriptEntryView entry={entry} key={entry.id} toolIndex={toolIndex} />
			))}
			{isRunning ? (
				<div className="activity-indicator">
					<Loader2 className="spin" size={14} />
					Agent active
				</div>
			) : null}
		</div>
	);
}

function TranscriptEntryView({
	entry,
	toolIndex
}: {
	entry: TranscriptEntry;
	toolIndex: ReturnType<typeof indexToolEntries>;
}) {
	const item = entry.item;
	if (item.type === "turn_started") {
		return null;
	}
	if (item.type === "turn_finished") {
		if (item.outcome === "Graceful") return null;
		return (
			<SystemMessage
				tone={item.outcome === "Interrupted" ? "info" : "error"}
				text={`turn ${item.turn_id} ${item.outcome.toLowerCase()}`}
			/>
		);
	}
	if (item.type === "user_message") {
		return <UserBubble item={item} entryId={entry.id} />;
	}
	if (item.type === "assistant_message") {
		return <AssistantBlock item={item} toolResults={toolIndex.results} />;
	}
	if (item.type === "tool_result") {
		if (toolIndex.calls.has(item.tool_call_id)) return null;
		return <ToolResultCard item={item} entryId={entry.id} />;
	}
	if (item.type === "tool_call_started") {
		return null;
	}
	if (item.type === "compaction_summary") {
		return <SystemMessage tone="info" text="compacted history" />;
	}
	return null;
}

function UserBubble({ item, entryId }: { item: Extract<TranscriptItem, { type: "user_message" }>; entryId: string }) {
	return (
		<div className="message-row user-row">
			<EntryId entryId={entryId} />
			<div className="user-bubble">{contentBlocksToText(item.content)}</div>
		</div>
	);
}

function AssistantBlock({
	item,
	toolResults
}: {
	item: Extract<TranscriptItem, { type: "assistant_message" }>;
	toolResults: Map<string, ToolResultItem>;
}) {
	const text = assistantMessageText(item);
	return (
		<div className="message-row assistant-row">
			<div className={`assistant-block ${text ? "has-copy" : ""}`}>
				{item.items.map((assistantItem, index) => {
					if (assistantItem.type === "text") {
						return <MarkdownText text={assistantItem.text} key={index} />;
					}
					if (assistantItem.type === "tool_call") {
						return (
							<ToolCard
								key={assistantItem.id}
								toolName={assistantItem.tool_name}
								toolId={assistantItem.id}
								argsJson={assistantItem.args_json}
								result={toolResults.get(assistantItem.id)}
							/>
						);
					}
					return null;
				})}
				{text ? <AssistantCopyButton text={text} /> : null}
			</div>
		</div>
	);
}

function AssistantCopyButton({ text }: { text: string }) {
	const [copied, setCopied] = useState(false);
	const copy = () => {
		void navigator.clipboard?.writeText(text)
			.then(() => {
				setCopied(true);
				window.setTimeout(() => setCopied(false), 1200);
			})
			.catch(() => undefined);
	};
	return (
		<button type="button" className="assistant-copy-button" onClick={copy} title="Copy assistant message" aria-label="Copy assistant message">
			{copied ? <Check size={13} /> : <Copy size={13} />}
		</button>
	);
}

function MarkdownText({ text }: { text: string }) {
	return (
		<div className="assistant-markdown">
			<ReactMarkdown
				rehypePlugins={[rehypeRaw]}
				remarkPlugins={[remarkGfm]}
				components={{
					a: ({ href, children, ...props }) => (
						<a href={href} target="_blank" rel="noreferrer" {...props}>
							{children}
						</a>
					)
				}}
			>
				{text}
			</ReactMarkdown>
		</div>
	);
}

function ToolResultCard({ item, entryId }: { item: Extract<TranscriptItem, { type: "tool_result" }>; entryId: string }) {
	return (
		<div className="message-row tool-row">
			<EntryId entryId={entryId} />
			<ToolCard toolName={item.tool_name} toolId={item.tool_call_id} result={item} />
		</div>
	);
}

function ToolCard({
	toolName,
	toolId,
	argsJson,
	result
}: {
	toolName: string;
	toolId: string;
	argsJson?: string;
	result?: ToolResultItem;
}) {
	const [expanded, setExpanded] = useState(false);
	const input = parseToolInput(argsJson);
	const status = result ? result.status : "Running";
	const ok = result?.status === "Success";
	const failed = !!result && !ok;
	const header = formatToolHeader(toolName, input);

	return (
		<div className={`tool-card ${failed ? "error" : ok ? "ok" : "running"}`}>
			<button className="tool-card-toggle" type="button" onClick={() => setExpanded((open) => !open)}>
				<span className="tool-status-icon" aria-hidden="true">
					{!result ? (
						<Loader2 className="spin" size={14} />
					) : failed ? (
						<AlertTriangle size={14} />
					) : (
						<Check size={14} />
					)}
				</span>
				<Wrench size={13} className="tool-wrench" />
				<span className="tool-title">{header}</span>
				<span className="tool-status">{status.toLowerCase()}</span>
				<ChevronDown size={14} className={`tool-chevron ${expanded ? "open" : ""}`} />
			</button>
			{expanded ? (
				<div className="tool-card-body">
					{input ? (
						<div className="tool-section">
							<div className="tool-section-label">input</div>
							<pre>{JSON.stringify(input, null, 2)}</pre>
						</div>
					) : null}
					{result ? (
						<div className="tool-section">
							<div className="tool-section-label">output</div>
							<ToolOutput result={result} />
						</div>
					) : (
						<div className="tool-pending">waiting for tool result</div>
					)}
					<div className="tool-call-id">id {toolId}</div>
				</div>
			) : null}
		</div>
	);
}

function ToolOutput({ result }: { result: ToolResultItem }) {
	const output = result.output || "(empty)";
	const lines = output.split("\n");
	const isLong = lines.length > 28;
	const display = isLong ? `${lines.slice(0, 28).join("\n")}\n...` : output;
	return <pre className={result.status === "Success" ? "" : "tool-output-error"}>{display}</pre>;
}

function SystemMessage({ tone, text, entryId }: { tone: NoticeTone; text: string; entryId?: string }) {
	return (
		<div className={`system-message ${tone}`}>
			{entryId ? <EntryId entryId={entryId} inline /> : null}
			{text}
		</div>
	);
}

function EntryId({ entryId, inline = false }: { entryId: string; inline?: boolean }) {
	const copy = () => {
		void navigator.clipboard?.writeText(entryId).catch(() => undefined);
	};
	return (
		<button
			type="button"
			className={`entry-id ${inline ? "inline" : ""}`}
			onClick={copy}
			title={`copy ${entryId}`}
			aria-label={`copy entry id ${entryId}`}
			data-entry-id={entryId}
		>
			<span>{entryId.slice(0, 8)}</span>
			<Copy size={10} />
		</button>
	);
}

function indexToolEntries(entries: TranscriptEntry[]) {
	const results = new Map<string, ToolResultItem>();
	const calls = new Set<string>();
	for (const entry of entries) {
		const item = entry.item;
		if (item.type === "tool_result") {
			results.set(item.tool_call_id, item);
			continue;
		}
		if (item.type === "assistant_message") {
			for (const assistantItem of item.items) {
				if (assistantItem.type === "tool_call") calls.add(assistantItem.id);
			}
		}
	}
	return { results, calls };
}

function parseToolInput(argsJson?: string): Record<string, unknown> | null {
	if (!argsJson) return null;
	try {
		const value = JSON.parse(argsJson) as unknown;
		return value && typeof value === "object" && !Array.isArray(value)
			? (value as Record<string, unknown>)
			: null;
	} catch {
		return null;
	}
}

function formatToolHeader(name: string, input: Record<string, unknown> | null): string {
	if (!input) return name;
	const text = (key: string) => {
		const value = input[key];
		return typeof value === "string" ? value : "";
	};
	switch (name.toLowerCase()) {
		case "read":
			return `Read: ${text("path") || text("file_path")}`;
		case "write":
			return `Write: ${text("path") || text("file_path")}`;
		case "edit":
			return `Edit: ${text("path") || text("file_path")}`;
		case "bash":
			return `Bash: ${firstLine(text("command"))}`;
		default:
			return name;
	}
}
