// M9: per-session REPL console pane (bridge profile, contract v0.1).
// Interleaves user cells and the model's ipython tool cells (provenance
// badges), streams stdout/stderr live (ansi-ish), renders display_data
// png/jpeg, collapses error tracebacks. Input: Shift+Enter runs, up-arrow
// recalls history (persisted per session). User cells never start an agent
// turn and never enter model context; they queue at the shared kernel.
import { ChevronDown, CirclePlay, Loader2, SquareTerminal } from "lucide-react";
import { useEffect, useMemo, useRef, useState } from "react";
import { BridgeRequestError, BridgeTransportError } from "../client.ts";
import { replBusyCounts, type ReplCellView, type ReplOutputItem } from "../replStore.ts";
import { useBridge, useReplProjection } from "../useBridge.tsx";
import { AnsiText } from "./ansi.tsx";

const HISTORY_KEY = (sessionId: string) => `pi-relay:bridge:repl-history:${sessionId}`;
const HISTORY_CAP = 50;
const PANE_KEY = "pi-relay:bridge:repl-pane";

export function replPaneVisibleDefault(): boolean {
	try {
		return localStorage.getItem(PANE_KEY) !== "off";
	} catch {
		return true;
	}
}

export function setReplPaneVisible(visible: boolean): void {
	try {
		localStorage.setItem(PANE_KEY, visible ? "on" : "off");
	} catch {
		/* private mode */
	}
}

function loadHistory(sessionId: string): string[] {
	try {
		const raw = localStorage.getItem(HISTORY_KEY(sessionId));
		const parsed = raw ? JSON.parse(raw) : [];
		return Array.isArray(parsed) ? parsed.filter((v): v is string => typeof v === "string") : [];
	} catch {
		return [];
	}
}

function saveHistory(sessionId: string, history: string[]): void {
	try {
		localStorage.setItem(HISTORY_KEY(sessionId), JSON.stringify(history.slice(-HISTORY_CAP)));
	} catch {
		/* private mode */
	}
}

function StatusChip({ cell }: { cell: ReplCellView }) {
	switch (cell.status) {
		case "queued":
			return <span className="rounded bg-muted px-1.5 py-0.5 text-[10px] text-muted-foreground">queued{typeof cell.position === "number" && cell.position > 0 ? ` #${cell.position}` : ""}</span>;
		case "running":
			return (
				<span className="flex items-center gap-1 rounded bg-primary/10 px-1.5 py-0.5 text-[10px] text-primary">
					<Loader2 className="size-2.5 animate-spin" /> running
				</span>
			);
		case "done":
			return <span className="rounded bg-success/15 px-1.5 py-0.5 text-[10px] text-success">done{typeof cell.durationMs === "number" ? ` ${cell.durationMs}ms` : ""}</span>;
		case "error":
			return <span className="rounded bg-destructive/15 px-1.5 py-0.5 text-[10px] text-destructive">{cell.error?.ename ?? "error"}</span>;
		default:
			return <span className="rounded bg-muted px-1.5 py-0.5 text-[10px] text-muted-foreground">…</span>;
	}
}

function OutputView({ item }: { item: ReplOutputItem }) {
	if (item.stream === "display" && (item.mimeType === "image/png" || item.mimeType === "image/jpeg")) {
		if (item.truncated || !item.data) {
			return <div className="text-[11px] text-muted-foreground italic">[image {item.mimeType} truncated]</div>;
		}
		return <img className="my-1 max-h-64 max-w-full rounded border border-border" src={`data:${item.mimeType};base64,${item.data}`} alt="cell display output" />;
	}
	if (item.stream === "display") {
		return (
			<pre className="repl-out whitespace-pre-wrap break-words font-mono text-[11px] leading-4 text-foreground/90">
				<AnsiText text={item.data} />
			</pre>
		);
	}
	if (item.stream === "stderr") {
		return (
			<pre className="repl-err whitespace-pre-wrap break-words font-mono text-[11px] leading-4 text-warning">
				<AnsiText text={item.data} />
			</pre>
		);
	}
	if (item.stream === "error") {
		// collapsed traceback
		return (
			<details className="repl-trace group">
				<summary className="cursor-pointer select-none font-mono text-[11px] text-destructive">
					traceback <span className="text-muted-foreground">({item.data.split("\n").length} lines — click to expand)</span>
				</summary>
				<pre className="mt-1 whitespace-pre-wrap break-words rounded bg-destructive/5 p-2 font-mono text-[11px] leading-4 text-destructive">
					<AnsiText text={item.data} />
				</pre>
			</details>
		);
	}
	return (
		<pre className="repl-out whitespace-pre-wrap break-words font-mono text-[11px] leading-4 text-foreground/80">
			<AnsiText text={item.data} />
		</pre>
	);
}

function CellView({ cell }: { cell: ReplCellView }) {
	const isModel = cell.provenance === "model";
	return (
		<div className="repl-cell border-b border-border/40 px-3 py-1.5" data-cell-id={cell.cellId} data-provenance={cell.provenance ?? undefined} data-status={cell.status}>
			<div className="mb-0.5 flex items-center gap-2">
				<span
					className={`rounded px-1.5 py-0.5 text-[10px] font-semibold uppercase tracking-wide ${
						isModel ? "bg-purple-500/15 text-purple-400" : "bg-sky-500/15 text-sky-400"
					}`}
				>
					{isModel ? "model" : cell.provenance === "user" ? "you" : "cell"}
				</span>
				<StatusChip cell={cell} />
				<span className="ml-auto font-mono text-[9px] text-muted-foreground/60">{cell.cellId.slice(0, 18)}</span>
			</div>
			{cell.code !== null ? (
				<pre className="whitespace-pre-wrap break-words rounded bg-muted/40 px-2 py-1 font-mono text-[11px] leading-4 text-foreground">
					{cell.code}
					{cell.codeTruncated ? <span className="text-muted-foreground">… [echo truncated]</span> : null}
				</pre>
			) : null}
			{cell.outputs.map((item) => (
				<OutputView key={item.seq} item={item} />
			))}
			{cell.stdoutTruncated || cell.stderrTruncated ? (
				<div className="text-[10px] text-muted-foreground italic">[kernel-side output truncation]</div>
			) : null}
		</div>
	);
}

export function ReplPane({ sessionId, onClose }: { sessionId: string; onClose: () => void }) {
	const projection = useReplProjection(sessionId);
	const [text, setText] = useState("");
	const [notice, setNotice] = useState<string | null>(null);
	const [historyIndex, setHistoryIndex] = useState(-1);
	const historyRef = useRef<string[]>(loadHistory(sessionId));
	const scrollRef = useRef<HTMLDivElement>(null);
	const [stickToBottom, setStickToBottom] = useState(true);
	const { replStore } = useBridge();

	// reset per-session input state
	useEffect(() => {
		historyRef.current = loadHistory(sessionId);
		setHistoryIndex(-1);
		setText("");
		setNotice(null);
	}, [sessionId]);

	// autoscroll
	const cellCount = projection?.cells.length ?? 0;
	const lastSeq = projection?.watermark ?? 0;
	useEffect(() => {
		if (!stickToBottom) return;
		const el = scrollRef.current;
		if (el) el.scrollTop = el.scrollHeight;
	}, [cellCount, lastSeq, stickToBottom]);

	const busy = useMemo(() => (projection ? replBusyCounts(projection) : { queued: 0, running: 0 }), [projection]);

	const run = () => {
		const code = text.replace(/\s+$/, "");
		if (!code.trim()) return;
		setNotice(null);
		historyRef.current = [...historyRef.current, code];
		saveHistory(sessionId, historyRef.current);
		setHistoryIndex(-1);
		setText("");
		void replStore
			.execute(sessionId, code)
			.catch((err: unknown) => {
				if (err instanceof BridgeRequestError) setNotice(`${err.code}: ${err.detail}`);
				else if (err instanceof BridgeTransportError) setNotice(`${err.message} — outcome uncertain; the cell may have run.`);
				else setNotice(err instanceof Error ? err.message : String(err));
			});
	};

	const onKeyDown = (e: React.KeyboardEvent<HTMLTextAreaElement>) => {
		if (e.key === "Enter" && e.shiftKey) {
			e.preventDefault();
			run();
			return;
		}
		const history = historyRef.current;
		if (e.key === "ArrowUp" && (text === "" || historyIndex >= 0 || e.currentTarget.selectionStart === 0)) {
			if (history.length === 0) return;
			e.preventDefault();
			const nextIndex = historyIndex < 0 ? history.length - 1 : Math.max(0, historyIndex - 1);
			setHistoryIndex(nextIndex);
			setText(history[nextIndex] ?? text);
		} else if (e.key === "ArrowDown" && historyIndex >= 0) {
			e.preventDefault();
			const nextIndex = historyIndex + 1;
			if (nextIndex >= history.length) {
				setHistoryIndex(-1);
				setText("");
			} else {
				setHistoryIndex(nextIndex);
				setText(history[nextIndex] ?? "");
			}
		}
	};

	return (
		<section className="repl-pane flex max-h-[45%] min-h-[120px] flex-col border-t border-border" data-session={sessionId}>
			<div className="flex items-center gap-2 border-b border-border/60 px-3 py-1">
				<SquareTerminal className="size-3.5 text-muted-foreground" />
				<span className="text-[11px] font-semibold">REPL</span>
				<span className="text-[10px] text-muted-foreground">shared kernel namespace · never an agent turn</span>
				{busy.running > 0 || busy.queued > 0 ? (
					<span className="rounded bg-primary/10 px-1.5 py-0.5 text-[10px] text-primary">
						{busy.running > 0 ? "running" : ""}{busy.running > 0 && busy.queued > 0 ? " · " : ""}{busy.queued > 0 ? `${busy.queued} queued` : ""}
					</span>
				) : (
					<span className="rounded bg-muted px-1.5 py-0.5 text-[10px] text-muted-foreground">idle</span>
				)}
				<div className="ml-auto flex items-center gap-1">
					<button
						type="button"
						title="Close repl pane"
						onClick={onClose}
						className="rounded p-1 text-muted-foreground hover:bg-muted"
					>
						<ChevronDown className="size-3.5" />
					</button>
				</div>
			</div>
			<div
				ref={scrollRef}
				className="repl-console min-h-0 flex-1 overflow-y-auto"
				onScroll={(e) => {
					const el = e.currentTarget;
					setStickToBottom(el.scrollHeight - el.scrollTop - el.clientHeight < 40);
				}}
			>
				{projection && projection.cells.length > 0 ? (
					projection.cells.map((cell) => <CellView key={cell.cellId} cell={cell} />)
				) : (
					<div className="p-3 text-[11px] text-muted-foreground">
						No cells yet. Shift+Enter runs on the session kernel; the model&apos;s ipython cells show up here too.
					</div>
				)}
			</div>
			{notice ? <div className="border-t border-destructive/30 bg-destructive/10 px-3 py-1 text-[11px] text-destructive">{notice}</div> : null}
			<div className="flex items-end gap-2 border-t border-border/60 p-2">
				<textarea
					className="repl-input max-h-40 min-h-[34px] flex-1 resize-y rounded border border-border bg-background px-2 py-1.5 font-mono text-[12px] leading-4 outline-none focus:border-primary"
					placeholder="python cell — Shift+Enter to run, ↑ for history"
					value={text}
					onChange={(e) => {
						setText(e.target.value);
						setHistoryIndex(-1);
					}}
					onKeyDown={onKeyDown}
					rows={Math.min(6, Math.max(1, text.split("\n").length))}
				/>
				<button
					type="button"
					title="Run cell (Shift+Enter)"
					onClick={run}
					disabled={!text.trim()}
					className="flex items-center gap-1 rounded bg-primary px-2.5 py-1.5 text-[11px] font-medium text-primary-foreground disabled:opacity-40"
				>
					<CirclePlay className="size-3.5" /> Run
				</button>
			</div>
		</section>
	);
}
