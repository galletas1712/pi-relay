import { Archive, ArchiveRestore, SquarePen, Trash2 } from "lucide-react";
import { ActionMenu, type ActionMenuItem } from "./actionMenu.tsx";
import { firstDisabledReason } from "./connectionRecovery.tsx";
import {
	isArchivedSession,
	sessionStatusWithDelegations,
	sessionTitle,
	type SessionListItem
} from "./sessionList.ts";

export function SessionRow({
	session,
	selected,
	onSelect,
	onRename,
	onArchiveToggle,
	onDelete,
	mutationBlockedReason,
}: {
	session: SessionListItem;
	selected: boolean;
	onSelect: () => void;
	onRename: () => void;
	onArchiveToggle: () => void;
	onDelete: () => void;
	mutationBlockedReason?: string | null;
}) {
	const archived = isArchivedSession(session);
	const status = sessionStatusWithDelegations(session.activity, session.has_running_delegations ?? false);
	const idleAndQuiet = session.activity === "idle" && !(session.has_running_delegations ?? false);
	const canArchive = idleAndQuiet;
	const canDelete = idleAndQuiet;
	const title = sessionTitle(session);
	const statusLabel = `${archived ? "archived" : status} session`;
	return (
		<li className={`session-row ${selected ? "selected" : ""} ${archived ? "archived" : ""}`}>
			<button
				className="session-row-primary"
				type="button"
				onClick={onSelect}
				aria-current={selected ? "page" : undefined}
			>
				<span
					className={`status-rail ${archived ? "archived" : status}`}
					role="img"
					aria-label={statusLabel}
					title={statusLabel}
				/>
				<span className="session-main">
					<span className="session-title">{title}</span>
					<span className="session-sub">{session.provider.model}</span>
				</span>
			</button>
			<ActionMenu
				triggerLabel={`Open session actions for ${title}`}
				items={sessionMenuItems({ archived, canArchive, canDelete, onRename, onArchiveToggle, onDelete, mutationBlockedReason })}
			/>
		</li>
	);
}

const IDLE_SESSION_ACTION_REASON = "Available when the session and its subagents are idle.";

export function sessionMenuItems({
	archived,
	canArchive,
	canDelete,
	onRename,
	onArchiveToggle,
	onDelete,
	mutationBlockedReason,
}: {
	archived: boolean;
	canArchive: boolean;
	canDelete: boolean;
	onRename: () => void;
	onArchiveToggle: () => void;
	onDelete: () => void;
	mutationBlockedReason?: string | null;
}): ActionMenuItem[] {
	const ArchiveIcon = archived ? ArchiveRestore : Archive;
	return [
		{
			id: "rename",
			label: "Rename…",
			icon: <SquarePen size={15} aria-hidden />,
			focusDestination: "dialog",
			onSelect: onRename,
		},
		{
			id: archived ? "unarchive" : "archive",
			label: archived ? "Unarchive" : "Archive",
			icon: <ArchiveIcon size={15} aria-hidden />,
			disabled: !canArchive || !!mutationBlockedReason,
			disabledReason: firstDisabledReason(
				mutationBlockedReason,
				!canArchive && IDLE_SESSION_ACTION_REASON,
			) ?? undefined,
			onSelect: onArchiveToggle,
		},
		{
			id: "delete",
			label: "Delete…",
			icon: <Trash2 size={15} aria-hidden />,
			disabled: !canDelete,
			disabledReason: !canDelete ? IDLE_SESSION_ACTION_REASON : undefined,
			destructive: true,
			separatorBefore: true,
			focusDestination: "dialog",
			onSelect: onDelete,
		},
	];
}
