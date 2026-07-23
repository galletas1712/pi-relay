import { ArrowUp, Bot, PanelRightOpen } from "lucide-react";
import type { ReasoningEffort } from "./types.ts";
import type { SessionStatus } from "./sessionList.ts";
import { Button } from "@/components/ui/button";
import { NativeSelect, NativeSelectOption } from "@/components/ui/native-select";

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
	onToggleRight,
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
					className={`session-status-icon ${archived ? "archived" : (status ?? "idle")}`}
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
						<Button
							type="button"
							variant="ghost"
							size="icon-xs"
							className="parent-session-link"
							onClick={() => onSelectSession?.(parentSessionId)}
							title="Open parent conversation"
							aria-label="Open parent conversation"
						>
							<ArrowUp aria-hidden />
						</Button>
					) : null}
				</span>
			) : null}
			<div className="log-controls">
				<label className="header-select" title="Model">
					<span className="sr-only">Model</span>
					<NativeSelect
						size="sm"
						value={modelValue}
						disabled={modelDisabled}
						title="Model"
						aria-label="Model"
						onChange={(event) => onModelChange(event.target.value)}
					>
						{modelOptions.map((option) => (
							<NativeSelectOption key={option.id} value={option.id} title={option.description}>
								{option.label}
							</NativeSelectOption>
						))}
					</NativeSelect>
				</label>
				<label className="header-select compact" title="Reasoning effort">
					<span className="sr-only">Reasoning effort</span>
					<NativeSelect
						size="sm"
						value={reasoningEffort}
						disabled={reasoningDisabled}
						title="Reasoning effort"
						aria-label="Reasoning effort"
						onChange={(event) => onReasoningEffortChange(event.target.value as ReasoningEffort)}
					>
						{reasoningEfforts.map((effort) => (
							<NativeSelectOption key={effort} value={effort}>
								{effort}
							</NativeSelectOption>
						))}
					</NativeSelect>
				</label>
			</div>
			{rightOpen ? null : (
				<Button
					type="button"
					variant="ghost"
					size="icon-xs"
					onClick={onToggleRight}
					title="open inspector"
					aria-label="open inspector"
				>
					<PanelRightOpen />
				</Button>
			)}
		</div>
	);
}
