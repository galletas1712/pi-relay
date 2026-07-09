import { memo, useRef, type RefObject } from "react";
import ReactMarkdown from "react-markdown";
import rehypeRaw from "rehype-raw";
import remarkGfm from "remark-gfm";
import {
	AppDialog,
	DialogCloseButton,
	DialogDescription,
	DialogTitle,
} from "./dialog.tsx";
import { markdownComponents } from "./transcript.tsx";

export type SystemPromptDialogState = {
	loading: boolean;
	template: string;
	rendered: string | null;
	view: "rendered" | "template";
	error: string | null;
};

export function SystemPromptDialog({
	state,
	onChangeView,
	onClose,
	returnFocusFallbackRef,
}: {
	state: SystemPromptDialogState;
	onChangeView: (view: "rendered" | "template") => void;
	onClose: () => void;
	returnFocusFallbackRef?: RefObject<HTMLElement | null>;
}) {
	const titleRef = useRef<HTMLHeadingElement>(null);
	const text = state.view === "rendered" ? (state.rendered ?? "") : state.template;
	return (
		<AppDialog
			className="rename-dialog system-prompt-dialog"
			initialFocusRef={titleRef}
			returnFocusFallbackRef={returnFocusFallbackRef}
			onDismiss={onClose}
		>
			<div className="rename-dialog-head">
				<div className="rename-dialog-copy">
					<DialogTitle ref={titleRef} tabIndex={-1}>PI.md</DialogTitle>
					<DialogDescription>Rendered prompt and source template.</DialogDescription>
				</div>
				<DialogCloseButton label="close system prompt dialog" />
			</div>
			<div className="system-prompt-tabs" role="group" aria-label="PI.md view">
				<button
					type="button"
					className={state.view === "rendered" ? "selected" : ""}
					aria-pressed={state.view === "rendered"}
					onClick={() => onChangeView("rendered")}
					disabled={!state.rendered}
				>
					Rendered
				</button>
				<button
					type="button"
					className={state.view === "template" ? "selected" : ""}
					aria-pressed={state.view === "template"}
					onClick={() => onChangeView("template")}
				>
					Template
				</button>
			</div>
			<div className="system-prompt-body">
				{state.loading ? <p className="muted">Loading PI.md…</p> : null}
				{state.error ? <p className="error-text">{state.error}</p> : null}
				{!state.loading && !state.error ? (
					state.view === "rendered" ? <MarkdownView text={text} /> : <pre>{text}</pre>
				) : null}
			</div>
		</AppDialog>
	);
}

const MarkdownView = memo(function MarkdownView({ text }: { text: string }) {
	return (
		<div className="assistant-markdown system-prompt-markdown">
			<ReactMarkdown
				rehypePlugins={[rehypeRaw]}
				remarkPlugins={[remarkGfm]}
				components={markdownComponents}
			>
				{text}
			</ReactMarkdown>
		</div>
	);
});
