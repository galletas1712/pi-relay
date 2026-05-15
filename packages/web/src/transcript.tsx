import { memo, useCallback, useLayoutEffect, useMemo, useRef, useState, type UIEvent } from "react";
import { AlertTriangle, Check, ChevronDown, Copy, Globe2, Loader2, RotateCcw, Terminal, Wrench } from "lucide-react";
import rehypeRaw from "rehype-raw";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import { branchEntriesFor } from "./historyTargets.ts";
import { citationsFromReplay, hostedToolsFromReplay, localToolCallIdFromReplay, parsedProviderReplay, replayContainsAssistantText } from "./providerReplay.ts";
import { contentBlocksToText, firstLine } from "./text.ts";
import { assistantMessageText, buildTurnViews } from "./turnView.ts";
import type { ModelStepView } from "./turnView.ts";
import type { AssistantItem, NoticeTone, ReplayDisplay, TranscriptEntry, TranscriptItem } from "./types.ts";

type ToolResultItem = Extract<TranscriptItem, { type: "tool_result" }>;

type ScrollMetrics = Pick<HTMLDivElement, "clientHeight" | "scrollHeight" | "scrollTop">;
const STICKY_BOTTOM_EPSILON_PX = 1;
const ACTIVE_SESSION_SCROLL_KEY = "__active_session__";

export interface ScrollPositionSnapshot {
	scrollTop: number;
	sticky: boolean;
}

export function isScrolledAtBottom(node: ScrollMetrics): boolean {
	return node.scrollHeight - node.scrollTop - node.clientHeight <= STICKY_BOTTOM_EPSILON_PX;
}

function bottomScrollTop(node: ScrollMetrics): number {
	return Math.max(0, node.scrollHeight - node.clientHeight);
}

export function captureScrollPosition(node: ScrollMetrics): ScrollPositionSnapshot {
	return {
		scrollTop: node.scrollTop,
		sticky: isScrolledAtBottom(node)
	};
}

export function restoreScrollPosition(node: ScrollMetrics, position: ScrollPositionSnapshot): boolean {
	if (position.sticky) {
		node.scrollTop = bottomScrollTop(node);
	} else {
		node.scrollTop = position.scrollTop;
	}
	return isScrolledAtBottom(node);
}

export const MessageList = memo(function MessageList({
	entries,
	activeLeafId,
	isRunning,
	hasSession,
	sessionId,
	entriesSessionId,
	onResumeTurn,
	resumingTurnId
}: {
	entries: TranscriptEntry[];
	activeLeafId: string | null;
	isRunning: boolean;
	hasSession: boolean;
	sessionId?: string | null;
	entriesSessionId?: string | null;
	onResumeTurn?: (entryId: string, outcome: "Interrupted" | "Crashed") => void;
	resumingTurnId?: string | null;
}) {
	const scrollRef = useRef<HTMLDivElement | null>(null);
	const contentRef = useRef<HTMLDivElement | null>(null);
	const shouldStickToBottomRef = useRef(true);
	const activeScrollSessionKeyRef = useRef<string | null>(null);
	const activeScrollSessionCanSaveRef = useRef(false);
	const pendingScrollRestoreRef = useRef<{ key: string; position: ScrollPositionSnapshot } | null>(null);
	const scrollPositionsRef = useRef(new Map<string, ScrollPositionSnapshot>());
	const scrollSessionKey = hasSession ? (sessionId ?? ACTIVE_SESSION_SCROLL_KEY) : null;
	const entriesBelongToSelectedSession = !hasSession || !sessionId || entriesSessionId === sessionId;
	const visibleEntries = useMemo(
		() => (hasSession ? branchEntriesFor(entries, activeLeafId) : entries),
		[activeLeafId, entries, hasSession]
	);

	const scrollToBottom = useCallback(() => {
		const node = scrollRef.current;
		if (!node) return;
		node.scrollTop = bottomScrollTop(node);
		shouldStickToBottomRef.current = true;
		const key = activeScrollSessionKeyRef.current;
		if (key && activeScrollSessionCanSaveRef.current) scrollPositionsRef.current.set(key, { scrollTop: node.scrollTop, sticky: true });
	}, []);

	const handleScroll = useCallback((event: UIEvent<HTMLDivElement>) => {
		if (pendingScrollRestoreRef.current?.key === activeScrollSessionKeyRef.current) return;
		const position = captureScrollPosition(event.currentTarget);
		shouldStickToBottomRef.current = position.sticky;
		const key = activeScrollSessionKeyRef.current;
		if (key && activeScrollSessionCanSaveRef.current) scrollPositionsRef.current.set(key, position);
	}, []);

	useLayoutEffect(() => {
		if (activeScrollSessionKeyRef.current === scrollSessionKey) return;
		const node = scrollRef.current;
		const previousKey = activeScrollSessionKeyRef.current;
		if (previousKey && node && activeScrollSessionCanSaveRef.current) scrollPositionsRef.current.set(previousKey, captureScrollPosition(node));
		activeScrollSessionKeyRef.current = scrollSessionKey;
		activeScrollSessionCanSaveRef.current = false;
		if (!scrollSessionKey) {
			pendingScrollRestoreRef.current = null;
			shouldStickToBottomRef.current = true;
			return;
		}
		const fallbackPosition = node ? captureScrollPosition(node) : { scrollTop: 0, sticky: true };
		pendingScrollRestoreRef.current = {
			key: scrollSessionKey,
			position: scrollPositionsRef.current.get(scrollSessionKey) ?? fallbackPosition
		};
		shouldStickToBottomRef.current = false;
	}, [scrollSessionKey]);

	useLayoutEffect(() => {
		const pendingRestore = pendingScrollRestoreRef.current;
		if (pendingRestore?.key === scrollSessionKey) {
			if (!entriesBelongToSelectedSession) return;
			const node = scrollRef.current;
			if (node) {
				const sticky = restoreScrollPosition(node, pendingRestore.position);
				shouldStickToBottomRef.current = sticky;
				if (scrollSessionKey) scrollPositionsRef.current.set(scrollSessionKey, { scrollTop: node.scrollTop, sticky });
			}
			activeScrollSessionCanSaveRef.current = true;
			pendingScrollRestoreRef.current = null;
			return;
		}
		if (!entriesBelongToSelectedSession) return;
		activeScrollSessionCanSaveRef.current = true;
		if (!shouldStickToBottomRef.current) return;
		scrollToBottom();
	}, [entriesBelongToSelectedSession, isRunning, scrollSessionKey, scrollToBottom, visibleEntries]);

	useLayoutEffect(() => {
		if (!hasSession || typeof ResizeObserver === "undefined") return;
		const scroller = scrollRef.current;
		const content = contentRef.current;
		if (!scroller || !content) return;
		const observer = new ResizeObserver(() => {
			if (pendingScrollRestoreRef.current?.key === activeScrollSessionKeyRef.current) return;
			if (!entriesBelongToSelectedSession) return;
			if (shouldStickToBottomRef.current) scrollToBottom();
		});
		observer.observe(scroller);
		observer.observe(content);
		return () => observer.disconnect();
	}, [entriesBelongToSelectedSession, hasSession, scrollToBottom]);
	const toolIndex = useMemo(() => indexToolEntries(visibleEntries), [visibleEntries]);
	const modelStepsByEntry = useMemo(() => {
		const steps = new Map<string, ModelStepView>();
		for (const turn of buildTurnViews(visibleEntries)) {
			for (const step of turn.modelSteps) steps.set(step.entry.id, step);
		}
		return steps;
	}, [visibleEntries]);

	if (!hasSession) {
		return (
			<div className="message-scroll" ref={scrollRef} onScroll={handleScroll}>
				<div className="empty-state">
					<Terminal size={34} />
					<span>Select or create a session</span>
				</div>
			</div>
		);
	}

	return (
		<div className="message-scroll" ref={scrollRef} onScroll={handleScroll}>
			<div className="message-scroll-content" ref={contentRef}>
				{visibleEntries.map((entry) => (
					<TranscriptEntryView
						entry={entry}
						key={entry.id}
						modelStep={modelStepsByEntry.get(entry.id)}
						toolIndex={toolIndex}
						isActiveLeaf={entry.id === activeLeafId}
						isRunning={isRunning}
						onResumeTurn={onResumeTurn}
						resuming={entry.id === resumingTurnId}
					/>
				))}
				{isRunning ? (
					<div className="activity-indicator">
						<Loader2 className="spin" size={14} />
						Agent active
					</div>
				) : null}
			</div>
		</div>
	);
});

const TranscriptEntryView = memo(function TranscriptEntryView({
	entry,
	modelStep,
	toolIndex,
	isActiveLeaf,
	isRunning,
	onResumeTurn,
	resuming
}: {
	entry: TranscriptEntry;
	modelStep?: ModelStepView;
	toolIndex: ReturnType<typeof indexToolEntries>;
	isActiveLeaf: boolean;
	isRunning: boolean;
	onResumeTurn?: (entryId: string, outcome: "Interrupted" | "Crashed") => void;
	resuming: boolean;
}) {
	const item = entry.item;
	if (item.type === "turn_started") {
		return null;
	}
	if (item.type === "turn_finished") {
		if (item.outcome === "Graceful") return null;
		const canResume = isActiveLeaf && !isRunning && !!onResumeTurn;
		const actionLabel = item.outcome === "Interrupted" ? "Continue" : "Retry";
		const resumableOutcome = item.outcome as "Interrupted" | "Crashed";
		return (
			<SystemMessage
				tone={item.outcome === "Interrupted" ? "info" : "error"}
				text={`turn ${item.turn_id} ${item.outcome.toLowerCase()}`}
				action={
					canResume
						? {
								label: resuming ? "Starting..." : actionLabel,
								disabled: resuming,
								onClick: () => onResumeTurn?.(entry.id, resumableOutcome)
							}
						: undefined
				}
			/>
		);
	}
	if (item.type === "user_message") {
		return <UserBubble item={item} entryId={entry.id} />;
	}
	if (item.type === "assistant_message") {
		return <AssistantBlock item={item} providerReplay={entry.provider_replay} modelStep={modelStep} toolResults={toolIndex.results} />;
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
});

const UserBubble = memo(function UserBubble({ item, entryId }: { item: Extract<TranscriptItem, { type: "user_message" }>; entryId: string }) {
	return (
		<div className="message-row user-row">
			<EntryId entryId={entryId} />
			<div className="user-bubble">{contentBlocksToText(item.content)}</div>
		</div>
	);
});

const AssistantBlock = memo(function AssistantBlock({
	item,
	providerReplay,
	modelStep,
	toolResults
}: {
	item: Extract<TranscriptItem, { type: "assistant_message" }>;
	providerReplay?: TranscriptEntry["provider_replay"];
	modelStep?: ModelStepView;
	toolResults: Map<string, ToolResultItem>;
}) {
	const text = assistantMessageText(item);
	const phase = modelStep?.phase ?? "unknown";
	const renderParts = assistantRenderParts(item.items, providerReplay);
	const citations = citationsFromReplay(providerReplay);
	return (
		<div className="message-row assistant-row">
			<div className={`assistant-block phase-${phase} ${text ? "has-copy" : ""}`}>
				{renderParts.map((part) => {
					if (part.type === "text") {
						return <MarkdownText text={part.item.text} key={part.key} />;
					}
					if (part.type === "tool_call") {
						return (
							<ToolCard
								key={part.item.id}
								toolName={part.item.tool_name}
								toolId={part.item.id}
								argsJson={part.item.args_json}
								result={toolResults.get(part.item.id)}
								display={part.display}
							/>
						);
					}
					if (part.type === "hosted_tool") {
						return <HostedToolCard key={part.key} tool={part.tool} />;
					}
					return null;
				})}
				{citations.length ? <CitationList citations={citations} /> : null}
				{text ? <AssistantCopyButton text={text} /> : null}
			</div>
		</div>
	);
});

export type AssistantRenderPart =
	| { type: "text"; key: string; item: Extract<AssistantItem, { type: "text" }> }
	| { type: "tool_call"; key: string; item: Extract<AssistantItem, { type: "tool_call" }>; display?: ReplayDisplay | null }
	| { type: "hosted_tool"; key: string; tool: ReturnType<typeof hostedToolsFromReplay>[number] };

export function assistantRenderParts(items: AssistantItem[], providerReplay: TranscriptEntry["provider_replay"] | undefined): AssistantRenderPart[] {
	items = coalesceAdjacentTextItems(items);
	const parsedReplay = parsedProviderReplay(providerReplay);
	const hostedTools = hostedToolsFromReplay(providerReplay);
	if (parsedReplay.length === 0) {
		return items.map((item, index) => itemRenderPart(item, `item-${index}`));
	}

	const parts: AssistantRenderPart[] = [];
	const localToolById = new Map<string, Extract<AssistantItem, { type: "tool_call" }>>();
	for (const item of items) {
		if (item.type === "tool_call") localToolById.set(item.id, item);
	}
	const localDisplayById = new Map<string, ReplayDisplay | null | undefined>();
	for (const replayItem of parsedReplay) {
		const toolId = localToolCallIdFromReplay(replayItem.raw);
		if (toolId) localDisplayById.set(toolId, replayItem.display);
	}
	const hostedById = new Map(hostedTools.map((tool) => [tool.id, tool]));
	const renderedTextIndexes = new Set<number>();
	const renderedToolIds = new Set<string>();
	const renderedHostedIds = new Set<string>();

	const renderText = () => {
		items.forEach((item, index) => {
			if (item.type === "text" && !renderedTextIndexes.has(index)) {
				renderedTextIndexes.add(index);
				parts.push({ type: "text", key: `text-${index}`, item });
			}
		});
	};

	for (const replayItem of parsedReplay) {
		const hosted = replayItem.raw.id ? hostedById.get(replayItem.raw.id) : undefined;
		if (hosted && !renderedHostedIds.has(hosted.id)) {
			renderedHostedIds.add(hosted.id);
			parts.push({ type: "hosted_tool", key: `hosted-${hosted.id}`, tool: hosted });
			continue;
		}
		const toolId = localToolCallIdFromReplay(replayItem.raw);
		if (toolId) {
			const tool = localToolById.get(toolId);
			if (tool && !renderedToolIds.has(tool.id)) {
				renderedToolIds.add(tool.id);
				parts.push({ type: "tool_call", key: `tool-${tool.id}`, item: tool, display: localDisplayById.get(tool.id) });
			}
			continue;
		}
		if (replayContainsAssistantText(replayItem.raw)) {
			renderText();
		}
	}

	items.forEach((item, index) => {
		if (item.type === "text") {
			if (!renderedTextIndexes.has(index)) parts.push({ type: "text", key: `text-${index}`, item });
			return;
		}
		if (!renderedToolIds.has(item.id)) parts.push({ type: "tool_call", key: `tool-${item.id}`, item, display: localDisplayById.get(item.id) });
	});
	for (const tool of hostedTools) {
		if (!renderedHostedIds.has(tool.id)) parts.push({ type: "hosted_tool", key: `hosted-${tool.id}`, tool });
	}
	return parts;
}

function itemRenderPart(item: AssistantItem, key: string): AssistantRenderPart {
	return item.type === "text" ? { type: "text", key, item } : { type: "tool_call", key, item };
}

function coalesceAdjacentTextItems(items: AssistantItem[]): AssistantItem[] {
	const merged: AssistantItem[] = [];
	for (const item of items) {
		const last = merged.at(-1);
		if (item.type === "text" && last?.type === "text") {
			last.text += item.text;
		} else {
			merged.push(item.type === "text" ? { ...item } : item);
		}
	}
	return merged;
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

const MarkdownText = memo(function MarkdownText({ text }: { text: string }) {
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
});

function CitationList({ citations }: { citations: ReturnType<typeof citationsFromReplay> }) {
	return (
		<div className="citation-list" aria-label="Sources">
			<span className="citation-label">Sources</span>
			{citations.map((citation, index) => (
				<a
					className="citation-chip"
					href={citation.url}
					key={citation.id}
					target="_blank"
					rel="noreferrer"
					title={citation.citedText ?? citation.title}
				>
					<span className="citation-index">{index + 1}</span>
					<span className="citation-title">{citation.title}</span>
				</a>
			))}
		</div>
	);
}

const HostedToolCard = memo(function HostedToolCard({ tool }: { tool: ReturnType<typeof hostedToolsFromReplay>[number] }) {
	const [expanded, setExpanded] = useState(false);
	const failed = tool.status === "error";
	const ok = tool.status === "completed";
	return (
		<div className={`tool-card hosted ${failed ? "error" : ok ? "ok" : "running"}`}>
			<button className="tool-card-toggle" type="button" onClick={() => setExpanded((open) => !open)}>
				<span className="tool-status-icon" aria-hidden="true">
					{failed ? <AlertTriangle size={14} /> : ok ? <Check size={14} /> : <Loader2 className="spin" size={14} />}
				</span>
				<Globe2 size={13} className="tool-wrench" />
				<span className="tool-title">{formatDisplayHeader(tool.prettyName, tool.inputSummary)}</span>
				<span className="tool-status">{tool.status}</span>
				<ChevronDown size={14} className={`tool-chevron ${expanded ? "open" : ""}`} />
			</button>
			{expanded ? (
				<div className="tool-card-body">
					{tool.input ? (
						<div className="tool-section">
							<div className="tool-section-label">input</div>
							<pre>{JSON.stringify(tool.input, null, 2)}</pre>
						</div>
					) : null}
					{tool.output ? (
						<div className="tool-section">
							<div className="tool-section-label">output</div>
							<pre className={failed ? "tool-output-error" : ""}>{tool.output}</pre>
						</div>
					) : null}
					<div className="tool-call-id">
						{tool.provider} hosted tool {tool.id}
					</div>
				</div>
			) : null}
		</div>
	);
});

const ToolResultCard = memo(function ToolResultCard({ item, entryId }: { item: Extract<TranscriptItem, { type: "tool_result" }>; entryId: string }) {
	return (
		<div className="message-row tool-row">
			<EntryId entryId={entryId} />
			<ToolCard toolName={item.tool_name} toolId={item.tool_call_id} result={item} />
		</div>
	);
});

const ToolCard = memo(function ToolCard({
	toolName,
	toolId,
	argsJson,
	result,
	display
}: {
	toolName: string;
	toolId: string;
	argsJson?: string;
	result?: ToolResultItem;
	display?: ReplayDisplay | null;
}) {
	const [expanded, setExpanded] = useState(false);
	const input = parseToolInput(argsJson);
	const status = result ? result.status : "Running";
	const ok = result?.status === "Success";
	const failed = !!result && !ok;
	const editPreview = editToolPreview(toolName, input, result);
	const header = editPreview?.header ?? formatDisplayHeader(display?.pretty_name ?? toolName, display?.input_summary ?? null);
	const showResultOutput = result && (!editPreview?.hideSuccessOutput || result.status !== "Success");

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
					{editPreview ? (
						<EditToolView preview={editPreview} />
					) : input ? (
						<div className="tool-section">
							<div className="tool-section-label">input</div>
							<pre>{JSON.stringify(input, null, 2)}</pre>
						</div>
					) : null}
					{showResultOutput ? (
						<div className="tool-section">
							<div className="tool-section-label">output</div>
							<ToolOutput result={result} />
						</div>
					) : !result ? (
						<div className="tool-pending">waiting for tool result</div>
					) : null}
					{editPreview ? null : (
						<div className="tool-call-id">id {toolId}</div>
					)}
				</div>
			) : null}
		</div>
	);
});

export interface EditToolPreview {
	header: string;
	action: string;
	file: string | null;
	additions: number;
	deletions: number;
	kind: "diff" | "file";
	rows?: EditDiffRow[];
	content?: string;
	hideSuccessOutput: boolean;
}

interface EditDiffRow {
	kind: "add" | "remove" | "context";
	text: string;
}

export function editToolPreview(
	toolName: string,
	input: Record<string, unknown> | null,
	result?: ToolResultItem
): EditToolPreview | null {
	if (toolName === "apply_patch") {
		const patch = stringValue(input?.input);
		return patch ? applyPatchPreview(patch) : null;
	}

	if (toolName !== "str_replace_based_edit_tool" || !input) return null;
	const command = stringValue(input.command);
	const path = stringValue(input.path);
	if (command === "view") {
		return {
			header: path ? `Read ${baseName(path)}` : "Read file",
			action: "Read",
			file: path,
			additions: 0,
			deletions: 0,
			kind: "file",
			content: result?.output ?? "",
			hideSuccessOutput: true
		};
	}
	if (command === "create") {
		const fileText = stringValue(input.file_text);
		return fileText == null || path == null
			? null
			: diffPreview("Edited", path, linesToRows(fileText, "add"), true);
	}
	if (command === "str_replace") {
		const oldStr = stringValue(input.old_str);
		const newStr = stringValue(input.new_str) ?? "";
		return oldStr == null || path == null
			? null
			: diffPreview("Edited", path, [...linesToRows(oldStr, "remove"), ...linesToRows(newStr, "add")], true);
	}
	if (command === "insert") {
		const newStr = stringValue(input.new_str);
		const insertLine = typeof input.insert_line === "number" ? input.insert_line : null;
		return path == null || newStr == null
			? null
			: diffPreview(`Edited after line ${insertLine ?? "?"}`, path, linesToRows(newStr, "add"), true);
	}
	return null;
}

function EditToolView({ preview }: { preview: EditToolPreview }) {
	return (
		<div className="edit-preview">
			<div className="edit-preview-head">
				<span className="edit-preview-action">{preview.action}</span>
				{preview.file ? (
					<span className="edit-preview-file" title={preview.file}>
						{baseName(preview.file)}
					</span>
				) : null}
				{preview.additions ? <span className="edit-count add">+{preview.additions}</span> : null}
				{preview.deletions ? <span className="edit-count remove">-{preview.deletions}</span> : null}
			</div>
			{preview.kind === "file" ? (
				<pre className="tool-file">{preview.content || "(empty)"}</pre>
			) : (
				<div className="edit-diff">
					{(preview.rows ?? []).map((row, index) => (
						<div className={`edit-diff-row ${row.kind}`} key={`${row.kind}-${index}`}>
							<span className="edit-diff-marker">{diffMarker(row.kind)}</span>
							<span className="edit-diff-text">{row.text || " "}</span>
						</div>
					))}
				</div>
			)}
		</div>
	);
}

function applyPatchPreview(patch: string): EditToolPreview {
	const lines = patch.split("\n");
	const file = patchFile(lines);
	const rows: EditDiffRow[] = [];
	for (const line of lines) {
		if (!line || line.startsWith("*** Begin Patch") || line.startsWith("*** End Patch") || line.startsWith("*** Add File:") || line.startsWith("*** Update File:") || line.startsWith("*** Delete File:")) continue;
		if (line.startsWith("+")) rows.push({ kind: "add", text: line.slice(1) });
		else if (line.startsWith("-")) rows.push({ kind: "remove", text: line.slice(1) });
		else rows.push({ kind: "context", text: line });
	}
	return diffPreview("Edited", file, rows, true);
}

function diffPreview(action: string, file: string | null, rows: EditDiffRow[], hideSuccessOutput: boolean): EditToolPreview {
	const additions = rows.filter((row) => row.kind === "add").length;
	const deletions = rows.filter((row) => row.kind === "remove").length;
	const counts = `${additions ? ` +${additions}` : ""}${deletions ? ` -${deletions}` : ""}`;
	return {
		header: `${action} ${file ? baseName(file) : "file"}${counts}`,
		action,
		file,
		additions,
		deletions,
		kind: "diff",
		rows,
		hideSuccessOutput
	};
}

function patchFile(lines: string[]): string | null {
	for (const line of lines) {
		const match = line.match(/^\*\*\* (?:Add|Update|Delete) File: (.+)$/);
		if (match) return match[1];
	}
	return null;
}

function linesToRows(text: string, kind: EditDiffRow["kind"]): EditDiffRow[] {
	const withoutFinalNewline = text.endsWith("\n") ? text.slice(0, -1) : text;
	const lines = withoutFinalNewline.length ? withoutFinalNewline.split("\n") : [""];
	return lines.map((line) => ({ kind, text: line }));
}

function diffMarker(kind: EditDiffRow["kind"]): string {
	if (kind === "add") return "+";
	if (kind === "remove") return "-";
	return " ";
}

function ToolOutput({ result }: { result: ToolResultItem }) {
	const output = result.output || "(empty)";
	const lines = output.split("\n");
	const isLong = lines.length > 28;
	const display = isLong ? `${lines.slice(0, 28).join("\n")}\n...` : output;
	return <pre className={result.status === "Success" ? "" : "tool-output-error"}>{display}</pre>;
}

function SystemMessage({
	tone,
	text,
	entryId,
	action
}: {
	tone: NoticeTone;
	text: string;
	entryId?: string;
	action?: { label: string; disabled?: boolean; onClick: () => void };
}) {
	return (
		<div className={`system-message ${tone}`}>
			{entryId ? <EntryId entryId={entryId} inline /> : null}
			<span>{text}</span>
			{action ? (
				<button type="button" className="system-message-action" onClick={action.onClick} disabled={action.disabled}>
					<RotateCcw size={12} />
					{action.label}
				</button>
			) : null}
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

function stringValue(value: unknown): string | null {
	return typeof value === "string" ? value : null;
}

function baseName(path: string): string {
	return path.split(/[\\/]/).filter(Boolean).at(-1) ?? path;
}

function formatDisplayHeader(prettyName: string, inputSummary: string | null): string {
	return inputSummary ? `${prettyName}: ${firstLine(inputSummary)}` : prettyName;
}
