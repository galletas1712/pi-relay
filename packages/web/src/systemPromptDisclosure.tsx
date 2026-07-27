import { AlertTriangle, Loader2 } from "lucide-react";
import { memo, useCallback, useEffect, useRef, useState } from "react";
import { ConnectionBlockedReason } from "./connectionRecovery.tsx";
import { AssistantMarkdown } from "./transcript.tsx";

type PromptState = {
	loaded: boolean;
	loading: boolean;
	rendered: string | null;
	error: string | null;
};

const INITIAL_PROMPT_STATE: PromptState = {
	loaded: false,
	loading: false,
	rendered: null,
	error: null,
};

export const SystemPromptDisclosure = memo(function SystemPromptDisclosure({
	loadPrompt,
	remoteReadBlockedReason,
}: {
	loadPrompt: () => Promise<string | null>;
	remoteReadBlockedReason?: string | null;
}) {
	const [expanded, setExpanded] = useState(false);
	const [prompt, setPrompt] = useState(INITIAL_PROMPT_STATE);
	const requestGenerationRef = useRef(0);

	useEffect(() => () => {
		requestGenerationRef.current += 1;
	}, []);

	const fetchPrompt = useCallback(() => {
		const generation = ++requestGenerationRef.current;
		setPrompt((current) => ({
			...current,
			loading: true,
			error: null,
		}));
		void loadPrompt()
			.then((rendered) => {
				if (requestGenerationRef.current !== generation) return;
				setPrompt({
					loaded: true,
					loading: false,
					rendered,
					error: null,
				});
			})
			.catch((error) => {
				if (requestGenerationRef.current !== generation) return;
				setPrompt({
					loaded: false,
					loading: false,
					rendered: null,
					error: errorMessage(error),
				});
			});
	}, [loadPrompt]);

	const toggle = useCallback(() => {
		if (expanded) {
			requestGenerationRef.current += 1;
			setExpanded(false);
			setPrompt((current) => ({
				...current,
				loading: false,
				error: null,
			}));
			return;
		}
		setExpanded(true);
		if (!prompt.loaded) fetchPrompt();
	}, [expanded, fetchPrompt, prompt.loaded]);

	return (
		<div className="transcript-system-prompt">
			<div className="transcript-system-prompt-control">
				<button
					type="button"
					className="link-button"
					aria-expanded={expanded}
					disabled={!expanded && !prompt.loaded && !!remoteReadBlockedReason}
					onClick={toggle}
				>
					{expanded ? "Hide system prompt" : "See system prompt"}
				</button>
				{!expanded && !prompt.loaded ? (
					<ConnectionBlockedReason reason={remoteReadBlockedReason} />
				) : null}
			</div>
			{expanded ? (
				<div className="transcript-system-prompt-body">
					{prompt.loading ? (
						<div className="transcript-system-prompt-status muted" role="status">
							<Loader2 className="spin" size={16} aria-hidden />
							<span>Loading system prompt…</span>
						</div>
					) : null}
					{prompt.error ? (
						<div className="transcript-system-prompt-error" role="alert">
							<AlertTriangle size={16} aria-hidden />
							<span>{prompt.error}</span>
							<button
								type="button"
								className="secondary-button"
								disabled={!!remoteReadBlockedReason}
								onClick={fetchPrompt}
							>
								Retry
							</button>
							<ConnectionBlockedReason reason={remoteReadBlockedReason} />
						</div>
					) : null}
					{prompt.loaded && !prompt.loading ? (
						prompt.rendered ? (
							<AssistantMarkdown text={prompt.rendered} />
						) : (
							<p className="muted">No persisted system prompt is available.</p>
						)
					) : null}
				</div>
			) : null}
		</div>
	);
});

function errorMessage(error: unknown): string {
	return error instanceof Error ? error.message : String(error);
}
