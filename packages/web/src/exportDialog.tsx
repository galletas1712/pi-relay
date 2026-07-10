import { useEffect, useMemo, useRef, useState, type RefObject } from "react";
import { Download, FileText } from "lucide-react";
import {
	AppDialog,
	DialogClose,
	DialogCloseButton,
	DialogDescription,
	DialogTitle,
} from "./dialog.tsx";
import {
	assistantExportBlocks,
	buildExportBlocks,
	defaultSelectedAssistantIds,
	downloadMarkdown,
	exportPreview,
	exportTitle,
	formatExportMarkdown,
	type ExportBlock,
} from "./exportTranscript.ts";
import type { TranscriptEntry } from "./types.ts";

export function ExportDialog({
	entries,
	blocks: providedBlocks,
	onClose,
	onError,
	returnFocusFallbackRef,
}: {
	entries: TranscriptEntry[];
	blocks?: ExportBlock[];
	onClose: () => void;
	onError: (error: unknown) => void;
	returnFocusFallbackRef?: RefObject<HTMLElement | null>;
}) {
	const titleRef = useRef<HTMLHeadingElement>(null);
	const copyRunningRef = useRef(false);
	const blocks = useMemo(
		() => providedBlocks ?? buildExportBlocks(entries),
		[entries, providedBlocks],
	);
	const assistants = useMemo(() => assistantExportBlocks(blocks), [blocks]);
	const [selectedIds, setSelectedIds] = useState<Set<string>>(() => defaultSelectedAssistantIds(blocks));
	const [copying, setCopying] = useState(false);

	useEffect(() => {
		setSelectedIds(defaultSelectedAssistantIds(blocks));
	}, [blocks]);

	const markdown = useMemo(() => formatExportMarkdown(blocks, selectedIds), [blocks, selectedIds]);
	const selectedCount = selectedIds.size;
	const canExport = selectedCount > 0 && markdown.length > 0;

	const toggle = (entryId: string) => {
		setSelectedIds((current) => {
			const next = new Set(current);
			if (next.has(entryId)) next.delete(entryId);
			else next.add(entryId);
			return next;
		});
	};

	const selectAll = () => setSelectedIds(new Set(assistants.map((assistant) => assistant.entryId)));
	const selectLast = () => {
		const last = assistants.at(-1);
		setSelectedIds(last ? new Set([last.entryId]) : new Set());
	};
	const clear = () => setSelectedIds(new Set());

	const copy = async () => {
		if (copyRunningRef.current) return;
		copyRunningRef.current = true;
		setCopying(true);
		try {
			await navigator.clipboard.writeText(markdown);
			onClose();
		} catch (error) {
			onError(error);
		} finally {
			copyRunningRef.current = false;
			setCopying(false);
		}
	};

	const download = () => {
		try {
			downloadMarkdown(`conversation-export-${new Date().toISOString().slice(0, 10)}.md`, markdown);
			onClose();
		} catch (error) {
			onError(error);
		}
	};

	return (
		<AppDialog
			className="export-dialog"
			busy={copying}
			initialFocusRef={titleRef}
			returnFocusFallbackRef={returnFocusFallbackRef}
			onDismiss={onClose}
		>
			<div className="export-dialog-head">
				<span className="history-dialog-icon" aria-hidden="true"><FileText size={15} /></span>
				<div className="history-dialog-copy">
					<DialogTitle ref={titleRef} tabIndex={-1}>Export messages</DialogTitle>
					<DialogDescription>
						Select model steps from the current branch. User inputs from the containing turn are included once.
					</DialogDescription>
				</div>
				<DialogCloseButton label="close export dialog" disabled={copying} />
			</div>

			<div className="export-toolbar">
				<div className="export-count">{selectedCount} of {assistants.length} selected</div>
				<div className="export-toolbar-actions">
					<button type="button" className="secondary-button" onClick={selectAll} disabled={copying || assistants.length === 0}>Select all</button>
					<button type="button" className="secondary-button" onClick={selectLast} disabled={copying || assistants.length === 0}>Select last</button>
					<button type="button" className="secondary-button" onClick={clear} disabled={copying || selectedCount === 0}>Clear</button>
				</div>
			</div>

			<div className="export-options">
				{assistants.length === 0 ? (
					<div className="export-empty">No assistant messages to export.</div>
				) : assistants.map((assistant, index) => (
					<label className="export-option" key={assistant.entryId}>
						<input
							type="checkbox"
							checked={selectedIds.has(assistant.entryId)}
							onChange={() => toggle(assistant.entryId)}
							disabled={copying}
						/>
						<span className="export-option-main">
							<span className={`export-option-title phase-${assistant.phase}`}>{exportTitle(assistant, index)}</span>
							<span className="export-option-preview">{exportPreview(assistant.text)}</span>
						</span>
					</label>
				))}
			</div>

			<div className="export-actions">
				<DialogClose className="secondary-button" disabled={copying}>Cancel</DialogClose>
				<button type="button" className="secondary-button" onClick={copy} disabled={copying || !canExport}>
					{copying ? "Copying…" : "Copy to clipboard"}
				</button>
				<button type="button" className="primary-button" onClick={download} disabled={copying || !canExport}><Download size={14} />Download Markdown</button>
			</div>
		</AppDialog>
	);
}
