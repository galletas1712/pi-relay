import { Folder, FolderGit2, Plus } from "lucide-react";
import { useEffect, useRef, useState, type RefObject } from "react";
import {
	AppAlertDialog,
	AppDialog,
	DialogBody,
	DialogClose,
	DialogCloseButton,
	DialogDescription,
	DialogFooter,
	DialogHeader,
	DialogHeading,
	DialogTitle,
} from "./dialog.tsx";
import { ConnectionBlockedReason } from "./connectionRecovery.tsx";
import { sessionTitle, type SessionListItem } from "./sessionList.ts";
import type { ProjectWorkspace, Runtime } from "./types.ts";

export type WorkspaceDraft =
	| {
			kind: "git";
			workspace_dir: string;
			remote_url: string;
			remote_branch: string;
	  }
	| {
			kind: "local";
			workspace_dir: string;
			source_path: string;
	  };

type WorkspaceDraftPatch = {
	kind?: "git" | "local";
	workspace_dir?: string;
	remote_url?: string;
	remote_branch?: string;
	source_path?: string;
};

export type ProjectDialogState = {
	mode: "create" | "edit";
	projectId?: string;
	name: string;
	runtimeId?: string;
	workspaces: WorkspaceDraft[];
	saving: boolean;
};

export function workspaceDraftFromProject(workspace: ProjectWorkspace): WorkspaceDraft {
	const kind = workspace.kind ?? "git";
	if (kind === "local") {
		return {
			kind,
			workspace_dir: workspace.workspace_dir,
			source_path: workspace.source_path ?? "",
		};
	}
	return {
		kind: "git",
		workspace_dir: workspace.workspace_dir,
		remote_url: workspace.remote_url ?? "",
		remote_branch: workspace.remote_branch ?? "",
	};
}

export function newWorkspaceDraft(kind: "git" | "local" = "git"): WorkspaceDraft {
	return kind === "local"
		? { kind: "local", workspace_dir: "", source_path: "" }
		: { kind: "git", workspace_dir: "", remote_url: "", remote_branch: "main" };
}

function updateWorkspaceDraft(current: WorkspaceDraft, patch: WorkspaceDraftPatch): WorkspaceDraft {
	const nextKind = patch.kind ?? current.kind;
	if (nextKind === "local") {
		return {
			kind: "local",
			workspace_dir: patch.workspace_dir ?? current.workspace_dir,
			source_path: patch.source_path ?? (current.kind === "local" ? current.source_path : ""),
		};
	}
	return {
		kind: "git",
		workspace_dir: patch.workspace_dir ?? current.workspace_dir,
		remote_url: patch.remote_url ?? (current.kind === "git" ? current.remote_url : ""),
		remote_branch: patch.remote_branch ?? (current.kind === "git" ? current.remote_branch : "main"),
	};
}

export function projectWorkspacesFromDrafts(workspaces: WorkspaceDraft[]): ProjectWorkspace[] {
	return workspaces.map((workspace, index) => {
		if (!workspace.workspace_dir.trim()) throw new Error(`workspace ${index + 1}: name is required`);
		if (workspace.kind === "local") {
			if (!workspace.source_path.trim()) throw new Error(`workspace ${index + 1}: source path is required`);
			return {
				kind: "local",
				workspace_dir: workspace.workspace_dir.trim(),
				source_path: workspace.source_path.trim(),
			};
		}
		if (!workspace.remote_url.trim()) throw new Error(`workspace ${index + 1}: remote URL is required`);
		if (!workspace.remote_branch.trim()) throw new Error(`workspace ${index + 1}: branch is required`);
		return {
			kind: "git",
			workspace_dir: workspace.workspace_dir.trim(),
			remote_url: workspace.remote_url.trim(),
			remote_branch: workspace.remote_branch.trim(),
		};
	});
}

function useDialogAction(
	action: () => void | Promise<void>,
	externalBusy: boolean,
	settledFocusRef: RefObject<HTMLElement | null>,
) {
	const runningRef = useRef(false);
	const restoreSettledFocusRef = useRef(false);
	const [running, setRunning] = useState(false);
	const busy = externalBusy || running;

	useEffect(() => {
		if (busy || !restoreSettledFocusRef.current) return;
		restoreSettledFocusRef.current = false;
		settledFocusRef.current?.focus();
	}, [busy, settledFocusRef]);

	const complete = () => {
		runningRef.current = false;
		restoreSettledFocusRef.current = true;
		setRunning(false);
	};
	const run = () => {
		if (runningRef.current || externalBusy) return;
		runningRef.current = true;
		setRunning(true);
		try {
			void Promise.resolve(action()).then(complete, complete);
		} catch {
			// The dialog owns callback settlement so a synchronous failure has
			// the same recoverable focus and busy-state behavior as rejection.
			complete();
		}
	};

	return { busy, run };
}

export function RenameSessionDialog({
	value,
	onChange,
	onClose,
	onSubmit,
	mutationBlockedReason,
	returnFocusFallbackRef,
}: {
	value: string;
	onChange: (value: string) => void;
	onClose: () => void;
	onSubmit: () => void | Promise<void>;
	mutationBlockedReason?: string | null;
	returnFocusFallbackRef?: RefObject<HTMLElement | null>;
}) {
	const inputRef = useRef<HTMLInputElement>(null);
	const submitRef = useRef<HTMLButtonElement>(null);
	const { busy, run: submit } = useDialogAction(onSubmit, false, submitRef);

	return (
		<AppDialog
			className="rename-dialog"
			busy={busy}
			initialFocusRef={inputRef}
			returnFocusFallbackRef={returnFocusFallbackRef}
			onDismiss={onClose}
		>
			<DialogHeader>
				<DialogHeading>
					<DialogTitle>Rename session</DialogTitle>
					<DialogDescription className="sr-only">
						Change the title used to identify this session.
					</DialogDescription>
				</DialogHeading>
				<DialogCloseButton label="close rename dialog" disabled={busy} />
			</DialogHeader>
			<form
				onSubmit={(event) => {
					event.preventDefault();
					if (mutationBlockedReason) return;
					submit();
				}}
			>
				<DialogBody>
					<label className="rename-field">
						<span>Session title</span>
						<input
							ref={inputRef}
							value={value}
							onChange={(event) => onChange(event.target.value)}
							onFocus={(event) => event.currentTarget.select()}
							placeholder="Session title"
							required
							disabled={busy}
						/>
					</label>
				</DialogBody>
				<DialogFooter>
					<ConnectionBlockedReason reason={mutationBlockedReason} className="dialog-blocked-reason" />
					<DialogClose className="secondary-button" disabled={busy}>Cancel</DialogClose>
					<button ref={submitRef} type="submit" className="primary-button" disabled={busy || !!mutationBlockedReason} aria-busy={busy}>
						{busy ? "Saving…" : "Save"}
					</button>
				</DialogFooter>
			</form>
		</AppDialog>
	);
}

export function DeleteSessionDialog({
	session,
	deleting,
	onClose,
	onConfirm,
	mutationBlockedReason,
	returnFocusFallbackRef,
}: {
	session: SessionListItem;
	deleting: boolean;
	onClose: () => void;
	onConfirm: () => void | Promise<void>;
	mutationBlockedReason?: string | null;
	returnFocusFallbackRef?: RefObject<HTMLElement | null>;
}) {
	const confirmRef = useRef<HTMLButtonElement>(null);
	const { busy, run: confirm } = useDialogAction(onConfirm, deleting, confirmRef);
	const title = sessionTitle(session);
	return (
		<AppAlertDialog
			className="rename-dialog"
			busy={busy}
			returnFocusFallbackRef={returnFocusFallbackRef}
			onDismiss={onClose}
		>
			<DialogHeader>
				<DialogHeading>
					<DialogTitle>Delete session</DialogTitle>
				</DialogHeading>
				<DialogCloseButton label="close delete dialog" disabled={busy} />
			</DialogHeader>
			<DialogBody className="delete-dialog-body">
				<p>
					Delete <strong>{title}</strong> permanently?
				</p>
				<DialogDescription className="muted">
					This removes the transcript, queued inputs, actions, and events for this session. This cannot be undone.
				</DialogDescription>
			</DialogBody>
			<DialogFooter>
				<ConnectionBlockedReason reason={mutationBlockedReason} className="dialog-blocked-reason" />
				<DialogClose className="secondary-button" disabled={busy}>Cancel</DialogClose>
				<button
					ref={confirmRef}
					type="button"
					className="primary-button destructive"
					onClick={() => {
						if (!mutationBlockedReason) confirm();
					}}
					disabled={busy || !!mutationBlockedReason}
					aria-busy={busy}
				>
					{busy ? "Deleting…" : "Delete"}
				</button>
			</DialogFooter>
		</AppAlertDialog>
	);
}

export function ProjectDialog({
	state,
	onChange,
	onClose,
	onSubmit,
	mutationBlockedReason,
	returnFocusFallbackRef,
	runtimes = [],
}: {
	state: ProjectDialogState;
	onChange: (patch: Partial<ProjectDialogState>) => void;
	onClose: () => void;
	onSubmit: () => void | Promise<void>;
	mutationBlockedReason?: string | null;
	returnFocusFallbackRef?: RefObject<HTMLElement | null>;
	runtimes?: Runtime[];
}) {
	const inputRef = useRef<HTMLInputElement>(null);
	const submitRef = useRef<HTMLButtonElement>(null);
	const { busy, run: submit } = useDialogAction(onSubmit, state.saving, submitRef);
	const title = state.mode === "create" ? "New project" : "Project settings";
	const updateWorkspace = (index: number, patch: WorkspaceDraftPatch) => {
		onChange({
			workspaces: state.workspaces.map((workspace, workspaceIndex) =>
				workspaceIndex === index ? updateWorkspaceDraft(workspace, patch) : workspace,
			),
		});
	};
	const removeWorkspace = (index: number) => {
		onChange({ workspaces: state.workspaces.filter((_, workspaceIndex) => workspaceIndex !== index) });
	};
	const addWorkspace = () => {
		onChange({ workspaces: [...state.workspaces, newWorkspaceDraft()] });
	};
	return (
		<AppDialog
			className="rename-dialog project-dialog"
			busy={busy}
			initialFocusRef={inputRef}
			returnFocusFallbackRef={returnFocusFallbackRef}
			onDismiss={onClose}
		>
			<DialogHeader>
				<DialogHeading>
					<DialogTitle>{title}</DialogTitle>
					<DialogDescription className="sr-only">
						Set the project name and the workspaces available to its sessions.
					</DialogDescription>
				</DialogHeading>
				<DialogCloseButton label="close project dialog" disabled={busy} />
			</DialogHeader>
			<form
				onSubmit={(event) => {
					event.preventDefault();
					if (mutationBlockedReason) return;
					submit();
				}}
			>
				<DialogBody>
					<label className="rename-field">
						<span>Project name</span>
						<input
							ref={inputRef}
							value={state.name}
							onChange={(event) => onChange({ name: event.target.value })}
							placeholder="Project name"
							required
							disabled={busy}
						/>
					</label>
					<label className="rename-field">
						<span>Runtime</span>
						<select
							value={state.runtimeId ?? ""}
							onChange={(event) => onChange({ runtimeId: event.target.value })}
							required
							disabled={busy || state.mode === "edit"}
						>
							<option value="">Select a runtime</option>
							{runtimes.map((runtime) => (
								<option key={runtime.runtime_id} value={runtime.runtime_id} disabled={!runtime.online}>
									{runtime.name} ({runtime.runtime_id}){runtime.online ? "" : " — offline"}
								</option>
							))}
						</select>
					</label>
					<div className="workspace-editor">
						<div className="workspace-editor-head">
							<span>Workspaces</span>
							<button
								type="button"
								className="icon-button"
								onClick={addWorkspace}
								disabled={busy}
								aria-label="add workspace"
								title="Add workspace"
							>
								<Plus size={14} aria-hidden />
							</button>
						</div>
						<div className="workspace-editor-list">
							{state.workspaces.map((workspace, index) => (
								<div className="workspace-card" key={index}>
									<div className="workspace-card-head">
										{workspace.kind === "git" ? <FolderGit2 size={14} /> : <Folder size={14} />}
										<span>{workspace.kind === "git" ? "Git repo" : "Local folder"}</span>
									</div>
									<div className="workspace-row">
										<label>
											<span>Type</span>
											<select
												value={workspace.kind}
												onChange={(event) => updateWorkspace(index, { kind: event.target.value as "git" | "local" })}
												disabled={busy}
											>
												<option value="git">Git repo</option>
												<option value="local">Local folder</option>
											</select>
										</label>
										<label>
											<span>Name</span>
											<input
												value={workspace.workspace_dir}
												onChange={(event) => updateWorkspace(index, { workspace_dir: event.target.value })}
												placeholder={workspace.kind === "local" ? "docs" : "pi-relay"}
												required
												disabled={busy}
											/>
										</label>
										<button
											type="button"
											className="secondary-button workspace-remove"
											onClick={() => removeWorkspace(index)}
											disabled={busy || state.workspaces.length <= 1}
										>
											Remove
										</button>
									</div>
									{workspace.kind === "local" ? (
										<label className="workspace-full-field">
											<span>Source path</span>
											<input
												value={workspace.source_path}
												onChange={(event) => updateWorkspace(index, { source_path: event.target.value })}
												placeholder="/Users/me/reference-docs"
												required
												disabled={busy}
											/>
										</label>
									) : (
										<div className="workspace-row git-fields">
											<label>
												<span>Remote URL</span>
												<input
													value={workspace.remote_url}
													onChange={(event) => updateWorkspace(index, { remote_url: event.target.value })}
													placeholder="https://github.com/me/pi-relay.git"
													required
													disabled={busy}
												/>
											</label>
											<label>
												<span>Branch</span>
												<input
													value={workspace.remote_branch}
													onChange={(event) => updateWorkspace(index, { remote_branch: event.target.value })}
													placeholder="main"
													required
													disabled={busy}
												/>
											</label>
										</div>
									)}
								</div>
							))}
						</div>
					</div>
				</DialogBody>
				<DialogFooter>
					<ConnectionBlockedReason reason={mutationBlockedReason} className="dialog-blocked-reason" />
					<DialogClose className="secondary-button" disabled={busy}>Cancel</DialogClose>
					<button ref={submitRef} type="submit" className="primary-button" disabled={busy || !!mutationBlockedReason} aria-busy={busy}>
						{busy ? "Saving…" : "Save"}
					</button>
				</DialogFooter>
			</form>
		</AppDialog>
	);
}
