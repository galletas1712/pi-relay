import { useEffect, useMemo, useState } from "react";
import { Download, FileText } from "lucide-react";
import {
	assistantExportBlocks,
	buildExportBlocks,
	downloadMarkdown,
	exportPreview,
	formatExportMarkdown
} from "./exportTranscript.ts";
import type { TranscriptEntry } from "./types.ts";

export function ExportDialog({
	entries,
	onClose,
	onCopied,
	onDownloaded,
	onError
}: {
	entries: TranscriptEntry[];
	onClose: () => void;
	onCopied: () => void;
	onDownloaded: () => void;
	onError: (error: unknown) => void;
}) {
	const blocks = useMemo(() => buildExportBlocks(entries), [entries]);
	const assistants = useMemo(() => assistantExportBlocks(blocks), [blocks]);
	const [selectedIds, setSelectedIds] = useState<Set<string>>(() => new Set(assistants.map((assistant) => assistant.entryId)));

	useEffect(() => {
		setSelectedIds(new Set(assistants.map((assistant) => assistant.entryId)));
	}, [assistants]);

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
		try {
			await navigator.clipboard.writeText(markdown);
			onCopied();
			onClose();
		} catch (error) {
			onError(error);
		}
	};

	const download = () => {
		try {
			downloadMarkdown(`conversation-export-${new Date().toISOString().slice(0, 10)}.md`, markdown);
			onDownloaded();
			onClose();
		} catch (error) {
			onError(error);
		}
	};

	return (
		<div className="modal-scrim" role="presentation" onMouseDown={onClose}>
			<div className="export-dialog" role="dialog" aria-modal="true" aria-labelledby="export-dialog-title" onMouseDown={(event) => event.stopPropagation()}>
				<div className="export-dialog-head">
					<span className="history-dialog-icon" aria-hidden="true"><FileText size={15} /></span>
					<div className="history-dialog-copy">
						<h2 id="export-dialog-title">Export messages</h2>
						<p>Select assistant messages from the current branch. Prior user messages are included once.</p>
					</div>
					<button className="icon-button tiny" type="button" onClick={onClose} aria-label="close export dialog">×</button>
				</div>

				<div className="export-toolbar">
					<div className="export-count">{selectedCount} of {assistants.length} selected</div>
					<div className="export-toolbar-actions">
						<button type="button" className="secondary-button" onClick={selectAll} disabled={assistants.length === 0}>Select all</button>
						<button type="button" className="secondary-button" onClick={selectLast} disabled={assistants.length === 0}>Select last</button>
						<button type="button" className="secondary-button" onClick={clear} disabled={selectedCount === 0}>Clear</button>
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
							/>
							<span className="export-option-main">
								<span className="export-option-title">Assistant message {index + 1}</span>
								<span className="export-option-preview">{exportPreview(assistant.text)}</span>
							</span>
						</label>
					))}
				</div>

				<div className="export-actions">
					<button type="button" className="secondary-button" onClick={onClose}>Cancel</button>
					<button type="button" className="secondary-button" onClick={copy} disabled={!canExport}>Copy to clipboard</button>
					<button type="button" className="primary-button" onClick={download} disabled={!canExport}><Download size={14} />Download Markdown</button>
				</div>
			</div>
		</div>
	);
}
