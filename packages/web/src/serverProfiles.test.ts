// @vitest-environment jsdom

import { beforeEach, describe, expect, it } from "vitest";
import {
	ACTIVE_SERVER_PROFILE_STORAGE_KEY,
	SERVER_PROFILES_STORAGE_KEY,
	ServerProfileStore,
	defaultServerUrl,
	validateServerUrl,
} from "./serverProfiles.ts";

beforeEach(() => {
	window.localStorage.clear();
	window.sessionStorage.clear();
});

describe("server profile validation", () => {
	it("requires WSS remotely and allows cleartext only on exact loopback hosts", () => {
		expect(validateServerUrl("wss://control.example.test/rpc")).toBe(
			"wss://control.example.test/rpc",
		);
		expect(validateServerUrl("ws://127.0.0.1:9876")).toBe("ws://127.0.0.1:9876/");
		expect(validateServerUrl("ws://localhost:9876")).toBe("ws://localhost:9876/");
		expect(validateServerUrl("ws://[::1]:9876")).toBe("ws://[::1]:9876/");
		expect(() => validateServerUrl("ws://control.example.test")).toThrow("only for loopback");
		expect(() => validateServerUrl("wss://user:secret@control.example.test")).toThrow(
			"must not contain credentials",
		);
	});

	it("seeds only loopback development and leaves remote static first run in setup", () => {
		expect(defaultServerUrl({ hostname: "localhost" })).toBe("ws://127.0.0.1:8787/");
		expect(defaultServerUrl({ hostname: "127.0.0.1" })).toBe("ws://127.0.0.1:8787/");
		expect(defaultServerUrl({ hostname: "::1" })).toBe("ws://127.0.0.1:8787/");
		expect(defaultServerUrl({ hostname: "static.example.test" })).toBeNull();
	});
});

describe("ServerProfileStore", () => {
	it("uses only the final v1 profile, selection, and profile-state keys", () => {
		expect(SERVER_PROFILES_STORAGE_KEY).toBe("piRelayServerProfiles:v1");
		expect(ACTIVE_SERVER_PROFILE_STORAGE_KEY).toBe("piRelayActiveServerProfile:v1");
		const storage = new MemoryStorage();
		const store = new ServerProfileStore(storage, new MemoryStorage(), null, idSequence());
		const profile = store.add("Only", "wss://only.example.test/").profiles[0];

		store.storageFor(profile.id).setItem("draft", "remember me");

		expect(Array.from(storage.keys())).toEqual([
			"piRelayServerProfiles:v1",
			`piRelayServerState:v1:${profile.id}:draft`,
		]);
	});

	it("stores name and immutable URL with tab-local selection", () => {
		const store = new ServerProfileStore(
			window.localStorage,
			window.sessionStorage,
			"ws://127.0.0.1:8787",
			idSequence(),
		);
		const local = store.current().profiles[0];
		store.update(local.id, "Renamed local");
		const next = store.add("Tailnet", "wss://control.tail.test/");
		const remote = next.profiles.find((profile) => profile.name === "Tailnet")!;
		store.select(remote.id);

		expect(JSON.parse(window.localStorage.getItem(SERVER_PROFILES_STORAGE_KEY)!)).toEqual([
			{ id: "local", name: "Renamed local", url: "ws://127.0.0.1:8787/" },
			{ id: remote.id, name: "Tailnet", url: "wss://control.tail.test/" },
		]);
		expect(window.sessionStorage.getItem(ACTIVE_SERVER_PROFILE_STORAGE_KEY)).toBe(remote.id);
	});

	it("keeps profile-scoped state when renaming an immutable profile", () => {
		const store = profileStore();
		const profile = store.add("Only", "wss://only.example.test/").profiles[0];
		store.storageFor(profile.id).setItem("draft", "remember me");

		const next = store.update(profile.id, "Renamed").profiles[0];

		expect(next).toEqual({
			id: profile.id,
			name: "Renamed",
			url: profile.url,
		});
		expect(store.storageFor(profile.id).getItem("draft")).toBe("remember me");
	});

	it("removes a profile without scanning unrelated profile-scoped state", () => {
		const storage = new MemoryStorage();
		const store = new ServerProfileStore(storage, new MemoryStorage(), null, idSequence());
		const profile = store.add("Only", "wss://only.example.test/").profiles[0];
		store.storageFor(profile.id).setItem("draft", "unreachable");

		expect(store.remove(profile.id)).toEqual({ profiles: [], activeProfileId: null });
		expect(() => store.storageFor(profile.id)).toThrow("not found");
		expect(Array.from(storage.values()).some((value) => value === "unreachable")).toBe(true);
	});

	it("loads only exact id, name, and url records", () => {
		const storage = new MemoryStorage();
		storage.setItem(SERVER_PROFILES_STORAGE_KEY, JSON.stringify([
			{ id: "valid", name: "Current", url: "wss://current.example.test/" },
			{ id: "bad", name: "", url: "wss://bad.example.test/" },
			{
				id: "credential-revision",
				name: "Credential revision",
				url: "wss://credential-revision.example.test/",
				credentialRevision: 1,
			},
			{
				id: "has-credential",
				name: "Credential marker",
				url: "wss://has-credential.example.test/",
				hasCredential: true,
			},
			{
				id: "token",
				name: "Token",
				url: "wss://token.example.test/",
				token: "secret",
			},
			{
				id: "unknown",
				name: "Unknown",
				url: "wss://unknown.example.test/",
				unknown: true,
			},
		]));
		const store = new ServerProfileStore(storage, new MemoryStorage(), null);

		expect(store.current().profiles).toEqual([
			{ id: "valid", name: "Current", url: "wss://current.example.test/" },
		]);
	});
});

function profileStore(): ServerProfileStore {
	return new ServerProfileStore(new MemoryStorage(), new MemoryStorage(), null, idSequence());
}

function idSequence(): () => string {
	let next = 0;
	return () => `id-${++next}`;
}

class MemoryStorage implements Storage {
	private readonly data = new Map<string, string>();
	get length() {
		return this.data.size;
	}
	clear(): void {
		this.data.clear();
	}
	getItem(key: string): string | null {
		return this.data.get(key) ?? null;
	}
	key(index: number): string | null {
		return Array.from(this.data.keys())[index] ?? null;
	}
	removeItem(key: string): void {
		this.data.delete(key);
	}
	setItem(key: string, value: string): void {
		this.data.set(key, value);
	}
	values(): string[] {
		return Array.from(this.data.values());
	}
	keys(): string[] {
		return Array.from(this.data.keys());
	}
}
