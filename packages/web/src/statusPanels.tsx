import { ArrowUp, Bot, PanelRightOpen } from "lucide-react";
import type { ReasoningEffort } from "./types.ts";
import type { SessionStatus } from "./sessionList.ts";

export function LogHeader({
	archived,
	status,
	title,
	parentSessionId,
	modelOptions,
	modelValue,
	modelDisabled,
	reasoningDisabled = false,
	reasoningEfforts,
	reasoningEffort,
	onModelChange,
	onReasoningEffortChange,
	onSelectSession,
	rightOpen,
	onToggleRight
}: {
	archived: boolean;
	status: SessionStatus | null;
	title: string | null;
	parentSessionId?: string | null;
	modelOptions: { id: string; label: string; description?: string }[];
	modelValue: string;
	modelDisabled: boolean;
	reasoningDisabled?: boolean;
	reasoningEfforts: ReasoningEffort[];
	reasoningEffort: ReasoningEffort;
	onModelChange: (value: string) => void;
	onReasoningEffortChange: (value: ReasoningEffort) => void;
	onSelectSession?: (sessionId: string) => void;
	rightOpen: boolean;
	onToggleRight: () => void;
}) {
	const statusLabel = archived ? "archived session" : status ? `${status} session` : null;
	return (
		<div className="log-header">
			{title ? (
				<span
					className={`session-status-icon ${archived ? "archived" : status ?? "idle"}`}
					role="img"
					aria-label={statusLabel ?? undefined}
					title={statusLabel ?? undefined}
				>
					<Bot size={14} aria-hidden />
				</span>
			) : null}
			{title ? (
				<span className="log-title-group">
					<span className="log-session">{title}</span>
					{parentSessionId ? (
						<button
							className="parent-session-link"
							type="button"
							onClick={() => onSelectSession?.(parentSessionId)}
							title="Open parent conversation"
							aria-label="Open parent conversation"
						>
							<ArrowUp size={14} aria-hidden />
						</button>
					) : null}
				</span>
			) : null}
			<div className="log-controls">
				<label className="header-select" title="Model">
					<span className="sr-only">Model</span>
					<select
						value={modelValue}
						disabled={modelDisabled}
						title="Model"
						aria-label="Model"
						onChange={(event) => onModelChange(event.target.value)}
					>
						{modelOptions.map((option) => (
							<option key={option.id} value={option.id} title={option.description}>
								{option.label}
							</option>
						))}
					</select>
				</label>
				<label className="header-select compact">
					<span className="sr-only">Reasoning effort</span>
					<select
						value={reasoningEffort}
						disabled={reasoningDisabled}
						title="Reasoning effort"
						aria-label="Reasoning effort"
						onChange={(event) => onReasoningEffortChange(event.target.value as ReasoningEffort)}
					>
						{reasoningEfforts.map((effort) => (
							<option key={effort} value={effort}>
								{effort}
							</option>
						))}
					</select>
				</label>
			</div>
			{rightOpen ? null : (
				<button
					className="icon-button tiny"
					type="button"
					onClick={onToggleRight}
					title="open inspector"
					aria-label="open inspector"
				>
					<PanelRightOpen size={14} />
				</button>
			)}
		</div>
	);
}
