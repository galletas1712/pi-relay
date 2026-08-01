// @vitest-environment jsdom

import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { useQueryClient } from "@tanstack/react-query";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { AgentApi } from "./agentApi.ts";
import { ServerProfileStore } from "./serverProfiles.ts";
import { browserWorkspaceRouteHistory } from "./workspaceRoute.ts";

const boundary = vi.hoisted(() => ({
	clients: [] as {
		url: string;
		close: ReturnType<typeof vi.fn>;
	}[],
	queryClients: [] as unknown[],
}));

vi.mock("./rpc.ts", async (importOriginal) => {
	const actual = await importOriginal<typeof import("./rpc.ts")>();
	return {
		...actual,
		AgentRpcClient: class {
			readonly connect = vi.fn(() => Promise.resolve());
			readonly reconnect = vi.fn(() => Promise.resolve());
			readonly close = vi.fn();
			readonly isOpen = vi.fn(() => false);
			readonly onEvent = vi.fn(() => () => undefined);
			readonly onStatus = vi.fn(() => () => undefined);
			readonly request = vi.fn();

			constructor(readonly url: string) {
				boundary.clients.push(this);
			}
		},
	};
});

vi.mock("./App.tsx", async () => {
	const React = await import("react");
	return {
		App: ({ api, entityStorage }: { api: AgentApi; entityStorage: Storage }) => {
			const queryClient = useQueryClient();
			if (!boundary.queryClients.includes(queryClient)) {
				boundary.queryClients.push(queryClient);
			}
			React.useEffect(() => {
				void api.connect();
				return () => api.close();
			}, [api]);
			return <div data-testid="entity-draft">{entityStorage.getItem("draft")}</div>;
		},
	};
});

import { ServerApp } from "./serverApp.tsx";

beforeEach(() => {
	boundary.clients = [];
	boundary.queryClients = [];
	window.localStorage.clear();
	window.sessionStorage.clear();
	window.history.replaceState(null, "", "/");
});

afterEach(() => {
	cleanup();
	vi.restoreAllMocks();
});

describe("ServerApp immutable profile boundary", () => {
	it("replaces route, entity storage, query cache, and client when selecting another profile", async () => {
		const store = localStore();
		const remote = store.add("Remote", "wss://control.example.test/socket")
			.profiles.find((profile) => profile.name === "Remote")!;
		store.storageFor("local").setItem("draft", "local draft");
		store.storageFor(remote.id).setItem("draft", "remote draft");
		render(<ServerApp store={store} />);

		window.history.replaceState(
			null,
			"",
			"/server/local/w/host/run/local-session/conversation/local-session",
		);
		fireEvent.change(screen.getByLabelText("Active server"), {
			target: { value: remote.id },
		});

		expect(screen.getByTestId("entity-draft").textContent).toBe("remote draft");
		expect(window.location.pathname).toBe(`/server/${remote.id}/`);
		await waitFor(() => expect(boundary.clients).toHaveLength(2));
		expect(boundary.clients[0].close).toHaveBeenCalledOnce();
		expect(boundary.clients[1]).toMatchObject({
			url: remote.url,
		});
		expect(boundary.queryClients[1]).not.toBe(boundary.queryClients[0]);
	});

	it("keeps the URL read-only while allowing rename without remounting", async () => {
		const store = localStore();
		render(<ServerApp store={store} />);
		fireEvent.click(screen.getByRole("button", { name: "Manage" }));
		fireEvent.click(screen.getByRole("button", { name: "Edit" }));

		expect(screen.queryByLabelText("WebSocket URL")).toBeNull();
		expect(screen.getByText(/To use another address, add a new server profile/)).toBeTruthy();
		fireEvent.change(screen.getByLabelText("Name"), { target: { value: "Renamed" } });
		fireEvent.click(screen.getByRole("button", { name: "Save" }));

		await waitFor(() => expect(store.current().profiles[0].name).toBe("Renamed"));
		expect(store.current().profiles[0].name).toBe("Renamed");
		expect(store.current().profiles[0].url).toBe("ws://127.0.0.1:8787/");
		expect(boundary.clients).toHaveLength(1);
		expect(boundary.clients[0].close).not.toHaveBeenCalled();
	});

	it("selects the next profile root when removing the active profile", async () => {
		const store = localStore();
		const remote = store.add("Remote", "wss://remote.example.test/")
			.profiles.find((profile) => profile.name === "Remote")!;
		render(<ServerApp store={store} />);
		window.history.replaceState(
			null,
			"",
			"/server/local/w/host/run/old/conversation/old",
		);
		fireEvent.click(screen.getByRole("button", { name: "Manage" }));
		vi.spyOn(window, "confirm").mockReturnValue(true);
		fireEvent.click(screen.getAllByRole("button", { name: "Remove" })[0]);

		await waitFor(() => expect(boundary.clients).toHaveLength(2));
		expect(store.current().activeProfileId).toBe(remote.id);
		expect(window.location.pathname).toBe(`/server/${remote.id}/`);
		expect(boundary.clients[0].close).toHaveBeenCalledOnce();
	});

	it("does not deliver another profile's popstate to the departing route adapter", () => {
		window.history.replaceState(null, "", "/server/local/w/host/run/a/conversation/a");
		const history = browserWorkspaceRouteHistory("local")!;
		const listener = vi.fn();
		const unsubscribe = history.subscribe(listener);

		window.history.replaceState(null, "", "/server/remote/w/host/run/b/conversation/b");
		window.dispatchEvent(new PopStateEvent("popstate"));

		expect(listener).not.toHaveBeenCalled();
		unsubscribe();
	});
});

function localStore(): ServerProfileStore {
	return new ServerProfileStore(
		window.localStorage,
		window.sessionStorage,
		"ws://127.0.0.1:8787",
		idSequence(),
	);
}

function idSequence(): () => string {
	let next = 0;
	return () => `id-${++next}`;
}
