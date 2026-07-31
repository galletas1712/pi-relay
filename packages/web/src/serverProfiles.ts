export const SERVER_PROFILES_STORAGE_KEY = "piRelayServerProfiles:v1";
export const ACTIVE_SERVER_PROFILE_STORAGE_KEY = "piRelayActiveServerProfile:v1";
const PROFILE_STORAGE_PREFIX = "piRelayServerState:v1:";
const DEFAULT_PROFILE_ID = "local";

export interface ServerProfile {
	id: string;
	name: string;
	url: string;
}

export interface ServerProfileSnapshot {
	profiles: ServerProfile[];
	activeProfileId: string | null;
}

export type ServerProfileStorage = Pick<Storage, "getItem" | "setItem" | "removeItem">;

export class ServerProfileStore {
	private profiles: ServerProfile[];
	private activeProfileId: string | null;
	private listeners = new Set<(snapshot: ServerProfileSnapshot) => void>();

	constructor(
		private readonly profileStorage: ServerProfileStorage,
		private readonly tabStorage: ServerProfileStorage,
		defaultUrl: string | null,
		private readonly createId: () => string = randomProfileId,
	) {
		const stored = readProfiles(profileStorage);
		if (stored === null && defaultUrl) {
			this.profiles = [{
				id: DEFAULT_PROFILE_ID,
				name: "Local",
				url: validateServerUrl(defaultUrl),
			}];
			writeProfiles(profileStorage, this.profiles);
		} else {
			this.profiles = stored ?? [];
		}
		const selected = readStorage(tabStorage, ACTIVE_SERVER_PROFILE_STORAGE_KEY);
		this.activeProfileId = profileExists(this.profiles, selected)
			? selected
			: this.profiles[0]?.id ?? null;
		this.persistSelection();
	}

	current(): ServerProfileSnapshot {
		return {
			profiles: this.profiles.map((profile) => ({ ...profile })),
			activeProfileId: this.activeProfileId,
		};
	}

	subscribe(listener: (snapshot: ServerProfileSnapshot) => void): () => void {
		this.listeners.add(listener);
		return () => this.listeners.delete(listener);
	}

	add(name: string, url: string): ServerProfileSnapshot {
		const profile = {
			id: uniqueProfileId(this.profiles, this.createId),
			name: validateProfileName(name),
			url: validateServerUrl(url),
		};
		writeProfiles(this.profileStorage, [...this.profiles, profile]);
		this.profiles = [...this.profiles, profile];
		if (!this.activeProfileId) this.activeProfileId = profile.id;
		this.persistSelection();
		return this.changed();
	}

	update(id: string, name: string): ServerProfileSnapshot {
		const profiles = [...this.profiles];
		const index = this.profileIndex(id);
		profiles[index] = {
			...profiles[index],
			name: validateProfileName(name),
		};
		writeProfiles(this.profileStorage, profiles);
		this.profiles = profiles;
		return this.changed();
	}

	remove(id: string): ServerProfileSnapshot {
		this.profileIndex(id);
		const profiles = this.profiles.filter((profile) => profile.id !== id);
		writeProfiles(this.profileStorage, profiles);
		this.profiles = profiles;
		if (this.activeProfileId === id) this.activeProfileId = profiles[0]?.id ?? null;
		this.persistSelection();
		return this.changed();
	}

	select(id: string): ServerProfileSnapshot {
		if (!profileExists(this.profiles, id)) throw new Error("server profile was not found");
		this.activeProfileId = id;
		this.persistSelection();
		return this.changed();
	}

	storageFor(profileId: string): Storage {
		if (!profileExists(this.profiles, profileId)) {
			throw new Error("server profile was not found");
		}
		return new NamespacedStorage(this.profileStorage, `${PROFILE_STORAGE_PREFIX}${profileId}:`);
	}

	private profileIndex(id: string): number {
		const index = this.profiles.findIndex((profile) => profile.id === id);
		if (index === -1) throw new Error("server profile was not found");
		return index;
	}

	private persistSelection(): void {
		try {
			if (this.activeProfileId) {
				this.tabStorage.setItem(ACTIVE_SERVER_PROFILE_STORAGE_KEY, this.activeProfileId);
			} else {
				this.tabStorage.removeItem(ACTIVE_SERVER_PROFILE_STORAGE_KEY);
			}
		} catch {
			// Selection remains valid in this tab even when sessionStorage is unavailable.
		}
	}

	private changed(): ServerProfileSnapshot {
		const next = this.current();
		for (const listener of this.listeners) listener(next);
		return next;
	}
}

export function validateProfileName(value: string): string {
	const name = value.trim();
	if (!name) throw new Error("server name is required");
	if (name.length > 80) throw new Error("server name must be 80 characters or fewer");
	return name;
}

export function validateServerUrl(value: string): string {
	const candidate = value.trim();
	if (!candidate) throw new Error("WebSocket URL is required");
	let parsed: URL;
	try {
		parsed = new URL(candidate);
	} catch {
		throw new Error("WebSocket URL is malformed");
	}
	if (parsed.protocol !== "ws:" && parsed.protocol !== "wss:") {
		throw new Error("WebSocket URL must use ws:// or wss://");
	}
	if (!parsed.hostname) throw new Error("WebSocket URL must include a host");
	if (parsed.username || parsed.password) {
		throw new Error("WebSocket URL must not contain credentials");
	}
	if (parsed.hash) throw new Error("WebSocket URL must not contain a fragment");
	if (parsed.protocol === "ws:" && !isLoopbackHost(parsed.hostname)) {
		throw new Error("ws:// is allowed only for loopback hosts; use wss:// remotely");
	}
	return parsed.href;
}

export function defaultServerUrl(location: Pick<Location, "hostname">): string | null {
	if (isLoopbackHost(location.hostname)) return "ws://127.0.0.1:8787/";
	return null;
}

function readProfiles(storage: ServerProfileStorage): ServerProfile[] | null {
	const raw = readStorage(storage, SERVER_PROFILES_STORAGE_KEY);
	if (!raw) return null;
	try {
		const parsed = JSON.parse(raw) as unknown;
		if (!Array.isArray(parsed)) return [];
		const profiles: ServerProfile[] = [];
		const ids = new Set<string>();
		for (const value of parsed) {
			if (!isStoredProfile(value) || ids.has(value.id)) continue;
			try {
				profiles.push({
					id: value.id,
					name: validateProfileName(value.name),
					url: validateServerUrl(value.url),
				});
				ids.add(value.id);
			} catch {
				// Ignore only the malformed profile.
			}
		}
		return profiles;
	} catch {
		return [];
	}
}

function writeProfiles(storage: ServerProfileStorage, profiles: ServerProfile[]): void {
	writeStorage(storage, SERVER_PROFILES_STORAGE_KEY, JSON.stringify(profiles));
}

class NamespacedStorage implements Storage {
	constructor(
		private readonly storage: ServerProfileStorage,
		private readonly prefix: string,
	) {}

	get length(): number {
		return this.keys().length;
	}

	clear(): void {
		for (const key of this.keys()) this.removeItem(key);
	}

	getItem(key: string): string | null {
		return readStorage(this.storage, `${this.prefix}${key}`);
	}

	key(index: number): string | null {
		return this.keys()[index] ?? null;
	}

	removeItem(key: string): void {
		this.storage.removeItem(`${this.prefix}${key}`);
	}

	setItem(key: string, value: string): void {
		writeStorage(this.storage, `${this.prefix}${key}`, value);
	}

	private keys(): string[] {
		const storage = this.storage as Partial<Storage>;
		if (typeof storage.length !== "number" || typeof storage.key !== "function") return [];
		const keys: string[] = [];
		for (let index = 0; index < storage.length; index += 1) {
			const key = storage.key(index);
			if (key?.startsWith(this.prefix)) keys.push(key.slice(this.prefix.length));
		}
		return keys;
	}
}

function profileExists(profiles: Pick<ServerProfile, "id">[], id: string | null): boolean {
	return !!id && profiles.some((profile) => profile.id === id);
}

function uniqueProfileId(
	profiles: Pick<ServerProfile, "id">[],
	createId: () => string,
): string {
	for (let attempt = 0; attempt < 10; attempt += 1) {
		const id = createId();
		if (validProfileId(id) && !profileExists(profiles, id)) return id;
	}
	throw new Error("could not create a unique server profile ID");
}

function randomProfileId(): string {
	return globalThis.crypto?.randomUUID?.() ?? `server-${Date.now()}-${Math.random().toString(36).slice(2)}`;
}

function validProfileId(value: string): boolean {
	return /^[A-Za-z0-9_-]{1,100}$/u.test(value);
}

function isStoredProfile(value: unknown): value is ServerProfile {
	return (
		isRecord(value) &&
		Object.keys(value).length === 3 &&
		Object.hasOwn(value, "id") &&
		Object.hasOwn(value, "name") &&
		Object.hasOwn(value, "url") &&
		typeof value.id === "string" &&
		validProfileId(value.id) &&
		typeof value.name === "string" &&
		typeof value.url === "string"
	);
}

function readStorage(storage: ServerProfileStorage, key: string): string | null {
	try {
		return storage.getItem(key);
	} catch {
		return null;
	}
}

function writeStorage(storage: ServerProfileStorage, key: string, value: string): void {
	try {
		storage.setItem(key, value);
	} catch {
		throw new Error("browser storage is unavailable or full");
	}
}

function isLoopbackHost(hostname: string): boolean {
	return (
		hostname === "127.0.0.1" ||
		hostname === "localhost" ||
		hostname === "::1" ||
		hostname === "[::1]"
	);
}

function isRecord(value: unknown): value is Record<string, unknown> {
	return typeof value === "object" && value !== null && !Array.isArray(value);
}
