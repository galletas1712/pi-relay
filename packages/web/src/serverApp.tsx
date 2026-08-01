import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { AlertCircle, Pencil, Plus, Server, Settings, Trash2 } from "lucide-react";
import { useEffect, useMemo, useRef, useState, type RefObject } from "react";
import { App } from "./App.tsx";
import { createAgentApi } from "./agentApi.ts";
import {
	AppDialog,
	DialogBody,
	DialogCloseButton,
	DialogDescription,
	DialogHeader,
	DialogHeading,
	DialogTitle,
} from "./dialog.tsx";
import { AgentRpcClient } from "./rpc.ts";
import {
	ServerProfileStore,
	defaultServerUrl,
	type ServerProfile,
	type ServerProfileSnapshot,
} from "./serverProfiles.ts";
import { browserWorkspaceRouteHistory } from "./workspaceRoute.ts";
import {
	AlertDialog,
	AlertDialogAction,
	AlertDialogCancel,
	AlertDialogContent,
	AlertDialogDescription,
	AlertDialogFooter,
	AlertDialogHeader,
	AlertDialogTitle,
} from "@/components/ui/alert-dialog";
import { Alert, AlertDescription } from "@/components/ui/alert";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import {
	Empty,
	EmptyContent,
	EmptyDescription,
	EmptyHeader,
	EmptyMedia,
	EmptyTitle,
} from "@/components/ui/empty";
import {
	Field,
	FieldDescription,
	FieldGroup,
	FieldLabel,
} from "@/components/ui/field";
import { Input } from "@/components/ui/input";
import {
	NativeSelect,
	NativeSelectOption,
} from "@/components/ui/native-select";
import { TooltipProvider } from "@/components/ui/tooltip";
import { cn } from "@/lib/utils";

type ProfileDraft = {
	id: string | null;
	name: string;
	url: string;
};

export function ServerApp({
	store = createBrowserProfileStore(),
}: {
	store?: ServerProfileStore;
}) {
	const initialSnapshot = useMemo(() => {
		const initial = selectProfileFromBrowserPath(store);
		normalizeInitialBrowserPath(initial);
		return initial;
	}, [store]);
	const [snapshot, setSnapshot] = useState(initialSnapshot);
	const snapshotRef = useRef(initialSnapshot);
	const [managerMode, setManagerMode] = useState<"browse" | "add" | null>(null);
	const manageButtonRef = useRef<HTMLButtonElement>(null);
	const activeProfile = snapshot.profiles.find(
		(profile) => profile.id === snapshot.activeProfileId,
	) ?? null;
	const activeRouteOwned =
		!!activeProfile && profileIdFromPath(window.location.pathname) === activeProfile.id;
	useEffect(() => {
		return store.subscribe((next) => {
			if (snapshotRef.current.activeProfileId !== next.activeProfileId) {
				const path = next.activeProfileId ? serverRootPath(next.activeProfileId) : "/";
				if (profileIdFromPath(window.location.pathname) !== next.activeProfileId) {
					try {
						replaceBrowserPath(path);
					} catch {
						window.location.reload();
					}
				}
			}
			snapshotRef.current = next;
			setSnapshot(next);
		});
	}, [initialSnapshot, store]);
	const selectProfile = (id: string) => {
		if (id !== snapshot.activeProfileId) store.select(id);
	};
	useEffect(() => {
		const onPopState = () => {
			const profileId = profileIdFromPath(window.location.pathname);
			const current = store.current();
			if (
				profileId &&
				profileId !== current.activeProfileId &&
				current.profiles.some((profile) => profile.id === profileId)
			) {
				setSnapshot(store.select(profileId));
				return;
			}
			if (!profileId || !current.profiles.some((profile) => profile.id === profileId)) {
				replaceBrowserPath(
					current.activeProfileId ? serverRootPath(current.activeProfileId) : "/",
				);
			}
		};
		window.addEventListener("popstate", onPopState);
		return () => window.removeEventListener("popstate", onPopState);
	}, [store]);

	return (
		<div className="server-app">
			<header className="server-bar">
				<div className="server-context">
					<Server aria-hidden />
					{snapshot.profiles.length > 0 ? (
						<NativeSelect
							className="server-context-select"
							value={snapshot.activeProfileId ?? ""}
							onChange={(event) => selectProfile(event.target.value)}
							aria-label="Active control server"
						>
							{snapshot.profiles.map((profile) => (
								<NativeSelectOption key={profile.id} value={profile.id}>
									{profile.name}
								</NativeSelectOption>
							))}
						</NativeSelect>
					) : (
						<span className="server-context-empty">No server configured</span>
					)}
				</div>
				<Button
					ref={manageButtonRef}
					type="button"
					variant="ghost"
					className="server-manage-button"
					aria-label="Manage control servers"
					onClick={() => setManagerMode("browse")}
				>
					<Settings data-icon="inline-start" aria-hidden />
					<span>Manage</span>
				</Button>
			</header>
			{activeProfile && activeRouteOwned ? (
				<ConnectedServer
					key={activeProfile.id}
					profile={activeProfile}
					entityStorage={store.storageFor(activeProfile.id)}
				/>
			) : (
				<section className="server-setup" aria-labelledby="server-setup-title">
					<Empty className="server-setup-content">
						<EmptyHeader>
							<EmptyMedia variant="icon">
								<Server aria-hidden />
							</EmptyMedia>
							<EmptyTitle>
								<h1 id="server-setup-title">Connect a control server</h1>
							</EmptyTitle>
							<EmptyDescription>
								Add the WebSocket address for the pi-relay control server you want
								to use.
							</EmptyDescription>
						</EmptyHeader>
						<EmptyContent>
							<Button type="button" onClick={() => setManagerMode("add")}>
								<Plus data-icon="inline-start" aria-hidden />
								Add control server
							</Button>
						</EmptyContent>
					</Empty>
				</section>
			)}
			{managerMode ? (
				<ServerManagerDialog
					snapshot={snapshot}
					initialMode={managerMode}
					onSelect={selectProfile}
					onClose={() => setManagerMode(null)}
					returnFocusFallbackRef={manageButtonRef}
					store={store}
				/>
			) : null}
		</div>
	);
}

function ConnectedServer({
	profile,
	entityStorage,
}: {
	profile: ServerProfile;
	entityStorage: Storage;
}) {
	const queryClient = useMemo(() => createQueryClient(), []);
	const api = useMemo(
		() => createAgentApi(new AgentRpcClient(profile.url)),
		[profile.url],
	);
	const routeHistory = useMemo(
		() => browserWorkspaceRouteHistory(profile.id),
		[profile.id],
	);
	useEffect(() => () => queryClient.clear(), [queryClient]);

	return (
		<QueryClientProvider client={queryClient}>
			<TooltipProvider>
				<App api={api} routeHistory={routeHistory} entityStorage={entityStorage} />
			</TooltipProvider>
		</QueryClientProvider>
	);
}

function ServerManagerDialog({
	snapshot,
	store,
	onSelect,
	onClose,
	initialMode,
	returnFocusFallbackRef,
}: {
	snapshot: ServerProfileSnapshot;
	store: ServerProfileStore;
	onSelect: (id: string) => void;
	onClose: () => void;
	initialMode: "browse" | "add";
	returnFocusFallbackRef: RefObject<HTMLElement | null>;
}) {
	const [draft, setDraft] = useState<ProfileDraft | null>(() =>
		initialMode === "add" ? emptyDraft() : null
	);
	const [pendingRemoval, setPendingRemoval] = useState<ServerProfile | null>(null);
	const [error, setError] = useState<string | null>(null);
	const nameInputRef = useRef<HTMLInputElement>(null);
	const addButtonRef = useRef<HTMLButtonElement>(null);
	const editorReturnFocusRef = useRef<HTMLElement | null>(null);
	const removalReturnFocusRef = useRef<HTMLButtonElement | null>(null);
	useEffect(() => {
		if (!draft) return;
		queueMicrotask(() => nameInputRef.current?.focus());
	}, [draft?.id]);
	const startEdit = (
		profile: ServerProfile | undefined,
		returnFocusTarget: HTMLElement,
	) => {
		setError(null);
		editorReturnFocusRef.current = returnFocusTarget;
		setDraft(
			profile
				? { id: profile.id, name: profile.name, url: profile.url }
				: emptyDraft(),
		);
	};
	const restoreEditorFocus = () => {
		const target = editorReturnFocusRef.current;
		editorReturnFocusRef.current = null;
		queueMicrotask(() => {
			if (target?.isConnected) {
				target.focus();
			} else {
				addButtonRef.current?.focus();
			}
		});
	};
	const cancelEdit = () => {
		setDraft(null);
		setError(null);
		restoreEditorFocus();
	};
	const save = () => {
		if (!draft) return;
		try {
			if (draft.id) {
				store.update(draft.id, draft.name);
			} else {
				store.add(draft.name, draft.url);
			}
			setDraft(null);
			setError(null);
			restoreEditorFocus();
		} catch (caught) {
			setError(errorMessage(caught));
		}
	};
	const remove = (profile: ServerProfile) => {
		try {
			store.remove(profile.id);
			setDraft(null);
			setError(null);
			setPendingRemoval(null);
		} catch (caught) {
			setError(errorMessage(caught));
		}
	};
	const restoreRemovalFocus = () => {
		const target = removalReturnFocusRef.current;
		removalReturnFocusRef.current = null;
		queueMicrotask(() => {
			if (target?.isConnected) {
				target.focus();
			} else {
				addButtonRef.current?.focus();
			}
		});
	};

	return (
		<AppDialog
			className={cn(
				"server-manager-dialog",
				pendingRemoval && "server-manager-dialog-confirming",
			)}
			initialFocusRef={draft ? nameInputRef : undefined}
			returnFocusFallbackRef={returnFocusFallbackRef}
			onDismiss={onClose}
		>
			<DialogHeader>
				<DialogHeading>
					<DialogTitle>Control servers</DialogTitle>
					<DialogDescription>
						Switch or manage connections saved in this browser.
					</DialogDescription>
				</DialogHeading>
				<DialogCloseButton label="Close server manager" />
			</DialogHeader>
			<DialogBody className="server-manager-body">
				{snapshot.profiles.length > 0 ? (
					<div className="server-profile-list">
						{snapshot.profiles.map((profile) => {
							const active = profile.id === snapshot.activeProfileId;
							return (
								<div
									className="server-profile-row"
									data-active={active || undefined}
									key={profile.id}
								>
									<button
										type="button"
										className="server-profile-select"
										aria-label={
											active
												? `${profile.name}, active control server`
												: `Switch to ${profile.name}`
										}
										aria-current={active ? "true" : undefined}
										onClick={() => {
											onSelect(profile.id);
											onClose();
										}}
									>
										<span className="server-profile-name">
											<strong>{profile.name}</strong>
											{active ? <Badge variant="secondary">Active</Badge> : null}
										</span>
										<code>{profile.url}</code>
									</button>
									<div className="server-profile-actions">
										<Button
											type="button"
											variant="ghost"
											aria-label={`Edit ${profile.name}`}
											onClick={(event) => startEdit(profile, event.currentTarget)}
										>
											<Pencil data-icon="inline-start" aria-hidden />
											<span className="server-action-label">Edit</span>
										</Button>
										<Button
											type="button"
											variant="ghost"
											aria-label={`Remove ${profile.name}`}
											onClick={(event) => {
												removalReturnFocusRef.current = event.currentTarget;
												setPendingRemoval(profile);
											}}
										>
											<Trash2 data-icon="inline-start" aria-hidden />
											<span className="server-action-label">Remove</span>
										</Button>
									</div>
								</div>
							);
						})}
					</div>
				) : draft ? null : (
					<Empty className="server-manager-empty">
						<EmptyHeader>
							<EmptyTitle>No control servers yet</EmptyTitle>
							<EmptyDescription>
								Add a server connection to start using pi-relay.
							</EmptyDescription>
						</EmptyHeader>
					</Empty>
				)}
				{draft ? (
					<form
						className="server-profile-form"
						onSubmit={(event) => {
							event.preventDefault();
							save();
						}}
					>
						<div className="server-profile-form-heading">
							<strong>{draft.id ? "Edit server" : "Add server"}</strong>
							<span>
								{draft.id
									? "Change the label used for this connection."
									: "Save a control server connection in this browser."}
							</span>
						</div>
						<FieldGroup className="server-profile-fields">
							<Field>
								<FieldLabel htmlFor="server-profile-name">Name</FieldLabel>
								<Input
									id="server-profile-name"
									ref={nameInputRef}
									value={draft.name}
									onChange={(event) => setDraft({ ...draft, name: event.target.value })}
									placeholder="Home control"
									maxLength={80}
									autoComplete="off"
									required
								/>
							</Field>
							<Field>
								<FieldLabel htmlFor="server-profile-url">WebSocket URL</FieldLabel>
								<Input
									id="server-profile-url"
									value={draft.url}
									onChange={(event) => setDraft({ ...draft, url: event.target.value })}
									placeholder="wss://control.example.ts.net/"
									inputMode="url"
									autoCapitalize="none"
									autoComplete="url"
									spellCheck={false}
									readOnly={!!draft.id}
									required
								/>
								{draft.id ? (
									<FieldDescription>
										Server URLs can’t be changed. Add a new profile to use a
										different address.
									</FieldDescription>
								) : null}
							</Field>
						</FieldGroup>
						<div className="server-profile-form-actions">
							<Button
								type="button"
								variant="outline"
								onClick={cancelEdit}
							>
								Cancel
							</Button>
							<Button type="submit">
								{draft.id ? "Save changes" : "Add server"}
							</Button>
						</div>
					</form>
				) : (
					<Button
						ref={addButtonRef}
						type="button"
						variant="outline"
						className="server-add-button"
						onClick={(event) => startEdit(undefined, event.currentTarget)}
					>
						<Plus data-icon="inline-start" aria-hidden />
						Add server
					</Button>
				)}
				{error ? (
					<Alert variant="destructive">
						<AlertCircle aria-hidden />
						<AlertDescription>{error}</AlertDescription>
					</Alert>
				) : null}
			</DialogBody>
			<AlertDialog
				open={!!pendingRemoval}
				onOpenChange={(open) => {
					if (!open) setPendingRemoval(null);
				}}
			>
				<AlertDialogContent
					className="server-removal-dialog"
					onCloseAutoFocus={(event) => {
						event.preventDefault();
						restoreRemovalFocus();
					}}
				>
					<AlertDialogHeader>
						<AlertDialogTitle>
							Remove {pendingRemoval?.name ?? "server"}?
						</AlertDialogTitle>
						<AlertDialogDescription>
							This removes the connection from this browser. Saved browser state tied
							to this profile will no longer be accessible, but data on the control
							server is not deleted.
						</AlertDialogDescription>
					</AlertDialogHeader>
					<AlertDialogFooter>
						<AlertDialogCancel autoFocus>Cancel</AlertDialogCancel>
						<AlertDialogAction
							variant="destructive"
							onClick={() => {
								if (pendingRemoval) remove(pendingRemoval);
							}}
						>
							Remove server
						</AlertDialogAction>
					</AlertDialogFooter>
				</AlertDialogContent>
			</AlertDialog>
		</AppDialog>
	);
}

function emptyDraft(): ProfileDraft {
	return { id: null, name: "", url: "" };
}

function createBrowserProfileStore(): ServerProfileStore {
	return new ServerProfileStore(
		window.localStorage,
		window.sessionStorage,
		defaultServerUrl(window.location),
	);
}

function createQueryClient(): QueryClient {
	return new QueryClient({
		defaultOptions: {
			queries: {
				staleTime: 15_000,
				gcTime: 10 * 60_000,
				refetchOnWindowFocus: false,
				retry: 1,
			},
		},
	});
}

function selectProfileFromBrowserPath(store: ServerProfileStore): ServerProfileSnapshot {
	const snapshot = store.current();
	const routeProfileId = profileIdFromPath(window.location.pathname);
	if (!routeProfileId) return snapshot;
	if (snapshot.profiles.some((profile) => profile.id === routeProfileId)) {
		return store.select(routeProfileId);
	}
	return snapshot;
}

function normalizeInitialBrowserPath(snapshot: ServerProfileSnapshot): void {
	const routeProfileId = profileIdFromPath(window.location.pathname);
	let path: string | null = null;
	if (!snapshot.activeProfileId) {
		if (window.location.pathname !== "/") path = "/";
	} else if (routeProfileId && routeProfileId !== snapshot.activeProfileId) {
		path = serverRootPath(snapshot.activeProfileId);
	} else if (!routeProfileId) {
		const suffix =
			window.location.pathname === "/"
				? "/"
				: `${window.location.pathname}${window.location.search}${window.location.hash}`;
		path = `/server/${encodeURIComponent(snapshot.activeProfileId)}${suffix}`;
	}
	if (!path) return;
	try {
		replaceBrowserPath(path);
	} catch {
		// A reload retries navigation without risking cross-profile route reuse.
	}
}

function profileIdFromPath(pathname: string): string | null {
	const match = /^\/server\/([^/]+)(?:\/|$)/u.exec(pathname);
	if (!match) return null;
	try {
		return decodeURIComponent(match[1]);
	} catch {
		return null;
	}
}

function serverRootPath(profileId: string): string {
	return `/server/${encodeURIComponent(profileId)}/`;
}

function replaceBrowserPath(path: string): void {
	window.history.replaceState(window.history.state ?? null, "", path);
}

function errorMessage(error: unknown): string {
	return error instanceof Error ? error.message : String(error);
}
