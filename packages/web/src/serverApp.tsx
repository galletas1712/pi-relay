import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { Server, Settings } from "lucide-react";
import { useEffect, useMemo, useRef, useState } from "react";
import { App } from "./App.tsx";
import { createAgentApi } from "./agentApi.ts";
import {
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
import { AgentRpcClient } from "./rpc.ts";
import {
	ServerProfileStore,
	defaultServerUrl,
	type ServerProfile,
	type ServerProfileSnapshot,
} from "./serverProfiles.ts";
import { browserWorkspaceRouteHistory } from "./workspaceRoute.ts";
import { TooltipProvider } from "@/components/ui/tooltip";

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
	const [managerOpen, setManagerOpen] = useState(false);
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
				<div className="server-bar-active">
					<Server size={14} aria-hidden />
					<span>Server</span>
					<strong>{activeProfile?.name ?? "Setup required"}</strong>
					{activeProfile ? <code>{activeProfile.url}</code> : null}
				</div>
				<div className="server-bar-actions">
					<label>
						<span className="sr-only">Active server</span>
						<select
							value={snapshot.activeProfileId ?? ""}
							onChange={(event) => selectProfile(event.target.value)}
							aria-label="Active server"
						>
							{snapshot.profiles.map((profile) => (
								<option key={profile.id} value={profile.id}>
									{profile.name}
								</option>
							))}
						</select>
					</label>
					<button
						type="button"
						className="server-manage-button"
						onClick={() => setManagerOpen(true)}
					>
						<Settings size={14} aria-hidden />
						<span>Manage</span>
					</button>
				</div>
			</header>
			{activeProfile && activeRouteOwned ? (
				<ConnectedServer
					key={activeProfile.id}
					profile={activeProfile}
					entityStorage={store.storageFor(activeProfile.id)}
				/>
			) : (
				<section className="server-setup" aria-labelledby="server-setup-title">
					<h1 id="server-setup-title">Control server setup</h1>
					<p>
						Add a control server name and WebSocket URL to begin.
					</p>
					<button type="button" className="primary-button" onClick={() => setManagerOpen(true)}>
						Add control server
					</button>
				</section>
			)}
			{managerOpen ? (
				<ServerManagerDialog
					snapshot={snapshot}
					onSelect={selectProfile}
					onClose={() => setManagerOpen(false)}
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
}: {
	snapshot: ServerProfileSnapshot;
	store: ServerProfileStore;
	onSelect: (id: string) => void;
	onClose: () => void;
}) {
	const [draft, setDraft] = useState<ProfileDraft | null>(null);
	const [error, setError] = useState<string | null>(null);
	const nameInputRef = useRef<HTMLInputElement>(null);
	const activeProfile = snapshot.profiles.find(
		(profile) => profile.id === snapshot.activeProfileId,
	) ?? null;
	const startEdit = (profile?: ServerProfile) => {
		setError(null);
		setDraft(
			profile
				? { id: profile.id, name: profile.name, url: profile.url }
				: { id: null, name: "", url: "" },
		);
	};
	const save = async () => {
		if (!draft) return;
		try {
			if (draft.id) {
				store.update(draft.id, draft.name);
			} else {
				store.add(draft.name, draft.url);
			}
			setDraft(null);
			setError(null);
		} catch (caught) {
			setError(errorMessage(caught));
		}
	};
	const remove = async (profile: ServerProfile) => {
		if (
			!window.confirm(
				`Remove server profile "${profile.name}"?`,
			)
		) {
			return;
		}
		try {
			store.remove(profile.id);
			setDraft(null);
			setError(null);
		} catch (caught) {
			setError(errorMessage(caught));
		}
	};

	return (
		<AppDialog
			className="server-manager-dialog"
			initialFocusRef={draft ? nameInputRef : undefined}
			onDismiss={onClose}
		>
			<DialogHeader>
				<DialogHeading>
					<DialogTitle>Control servers</DialogTitle>
					<DialogDescription>
						Profiles contain a name and immutable WebSocket URL.
					</DialogDescription>
				</DialogHeading>
				<DialogCloseButton label="close server manager" />
			</DialogHeader>
			<DialogBody className="server-manager-body">
				<div className="server-profile-list">
					{snapshot.profiles.map((profile) => (
						<div
							className={`server-profile-row ${profile.id === snapshot.activeProfileId ? "active" : ""}`}
							key={profile.id}
						>
							<button
								type="button"
								className="server-profile-select"
								onClick={() => {
									onSelect(profile.id);
									onClose();
								}}
							>
								<strong>{profile.name}</strong>
								<code>{profile.url}</code>
								{profile.id === snapshot.activeProfileId ? <span>Active in this tab</span> : null}
							</button>
							<button
								type="button"
								className="secondary-button"
								onClick={() => startEdit(profile)}
							>
								Edit
							</button>
							<button
								type="button"
								className="secondary-button destructive"
								onClick={() => void remove(profile)}
							>
								Remove
							</button>
						</div>
					))}
				</div>
				{draft ? (
					<form
						className="server-profile-form"
						onSubmit={(event) => {
							event.preventDefault();
							void save();
						}}
					>
						<label className="rename-field">
							<span>Name</span>
							<input
								ref={nameInputRef}
								value={draft.name}
								onChange={(event) => setDraft({ ...draft, name: event.target.value })}
								placeholder="Home control"
								maxLength={80}
								required
							/>
						</label>
						{draft.id ? (
							<p>
								Address: <code>{draft.url}</code>. To use another address, add a new
								server profile.
							</p>
						) : (
							<label className="rename-field">
								<span>WebSocket URL</span>
								<input
									value={draft.url}
									onChange={(event) => setDraft({ ...draft, url: event.target.value })}
									placeholder="wss://control.example.ts.net/"
									inputMode="url"
									required
								/>
							</label>
						)}
						<div className="server-profile-form-actions">
							<button
								type="button"
								className="secondary-button"
								onClick={() => {
									setDraft(null);
									setError(null);
								}}
							>
								Cancel
							</button>
							<button type="submit" className="primary-button">
								{draft.id ? "Save" : "Add server"}
							</button>
						</div>
					</form>
				) : (
					<button type="button" className="secondary-button" onClick={() => startEdit()}>
						Add server
					</button>
				)}
				{error ? <p className="error-text" role="alert">{error}</p> : null}
			</DialogBody>
			<DialogFooter>
				<span className="server-manager-current">
					Current: {activeProfile?.name ?? "none"}
				</span>
				<DialogClose className="secondary-button">Done</DialogClose>
			</DialogFooter>
		</AppDialog>
	);
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
